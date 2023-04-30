#[macro_use]
extern crate serde_json;
mod disk;
use crate::disk::FilesystemLogger;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1;
use cln_plugin::{Builder, Error, Plugin};
use lightning::ln::channelmanager::{ChannelCounterparty, ChannelDetails};
use lightning::ln::features::InitFeatures;
use rand::Rng;
use cln_rpc::model::{SendpayRequest, SendpayRoute, ListpeersResponse, WaitsendpayResponse};
use cln_rpc::primitives::{Amount, Secret, ShortChannelId};
use cln_rpc::{ClnRpc, Request};
use lightning::routing::router::RouteParameters;
use lightning::routing::router::{
	DefaultRouter, InFlightHtlcs, PaymentParameters, Route, RouteHop, Router,
};
use lightning::routing::scoring::{
	ProbabilisticScorerUsingTime, Score,
};
use lightning::util::ser::{Writeable, Writer};
use lightning_invoice::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::{env, fs};
use std::fs::File;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{Stdin, Stdout};
use bitcoin_hashes::sha256;
use bitcoin_hashes::Hash;
use cln_plugin::anyhow;
use lightning::ln::{PaymentSecret};
use config::Config;
use lightning::routing::gossip::{NetworkGraph, NodeId};
use lightning_rapid_gossip_sync::RapidGossipSync;
use rand_core::{OsRng, RngCore};
use tokio_util::sync::CancellationToken;
use cln_rpc::model::requests::ListpeersRequest;

#[derive(Clone)]
struct PlugState {
	networkgraph: Arc<NetworkGraph<Arc<FilesystemLogger>>>,
	scorer: Arc<
		Mutex<
			ProbabilisticScorerUsingTime<
				Arc<NetworkGraph<Arc<FilesystemLogger>>>,
				Arc<FilesystemLogger>,
				std::time::Instant,
			>,
		>,
	>,
	ldk_data_dir: String,
	logger: Arc<FilesystemLogger>,
	failed_channels: Arc<Mutex<Vec<u64>>>,
	payments: Arc<Mutex<HashMap<String, Payment>>>,
	pk: secp256k1::PublicKey,
	random_pay_hash: sha256::Hash,
	config: Conf,
	ct_token: Arc<Mutex<CancellationToken>>,
	// used to update scorer from successful payments and failed payments
	send_pay_routes: Arc<Mutex<HashMap<String, Vec<RouteHop>>>>,
}

#[derive(Clone, Debug)]
// used for retrying payments, 
struct Payment {
	bolt11: String,
	total_amount: Amount,
	failed_channels: Vec<u64>,
	created_at: u128,
	retry_count: u8,
	groupid: u64,
	payment_secret: PaymentSecret,
}


#[derive(Debug, Deserialize, Clone)]
struct Conf {
	network: String,
	ldk_data_dir: String,
	rpc_path: String,
	mpp_pref: u8,
	probe_amount: u64,
	RapidGossipSync_URL: String,
}
#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
	let home_dir = match env::var("HOME") {
        Ok(value) => value,
        Err(_) => {
            log::error!("HOME not set");
			return Ok(())
        }
    };

	let pk = match env::var("PK") {
		Ok(value) => value,
		Err(_) => {
			log::error!("PK not set");
			panic!()
		}
	};

	let settings = Config::builder()
		.add_source(config::File::with_name(&format!("{}/.config/altpay.toml", home_dir)))
		.build()
		.expect("no config file found, please add one in ~/.config/altpay.toml");
	let mut config: Conf = settings.try_deserialize().unwrap();
	config.rpc_path = if config.network == "bitcoin" {
		format!("{}/.lightning/bitcoin/lightning-rpc", home_dir)}
	else {
		format!("{}/.lightning/testnet/lightning-rpc", home_dir)
	};

 	let ldk_data_dir = config.ldk_data_dir.clone();
	let network = if config.network == "bitcoin" {
		Network::Bitcoin
	} else if config.network == "testnet"{
		Network::Testnet
	} else {
		panic!("network not supported")
	};

	

	let failed_channels = Arc::new(Mutex::new(Vec::new()));
	let genesis = genesis_block(network).header.block_hash();
	let network_graph_path = format!("{}/network_graph", ldk_data_dir.clone());
	let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));
	// graph from disk
	let network_graph = Arc::new(disk::read_network(
		Path::new(&network_graph_path),
		genesis,
		logger.clone(),
	));
	// scorer from disk
	let scorer_path = format!("{}/scorer", ldk_data_dir.clone());
	let scorer = Arc::new(Mutex::new(disk::read_scorer(
		Path::new(&scorer_path),
		Arc::clone(&network_graph),
		Arc::clone(&logger),
	)));
	let mut rng = rand::thread_rng();
    let mut random_bytes = [0u8; 32];
    rng.fill(&mut random_bytes);

	//used for the probe
	let random_pay_hash = sha256::Hash::from_slice(&random_bytes[..]).unwrap();

	let payments = Arc::new(Mutex::new(HashMap::new()));
	let state = PlugState {
		networkgraph: network_graph,
		scorer,
		ldk_data_dir,
		logger,
		failed_channels,
		payments,
		pk: secp256k1::PublicKey::from_str(&pk).unwrap(),
		random_pay_hash,
		config,
		ct_token: Arc::new(Mutex::new(CancellationToken::new())),
		send_pay_routes: Arc::new(Mutex::new(HashMap::new())),
	};

	if let Some(plugin) =
		Builder::<PlugState, Stdin, Stdout>::new(tokio::io::stdin(), tokio::io::stdout())
			.rpcmethod("altpay", "optimized routing", altpay_method)
			.rpcmethod("probe", "probes netwrok", probe)
			.rpcmethod("network_probe", "probes netwrok", network_probe)
			.rpcmethod(
				"set_penalty",
				"sets penalty for a given publickey and a value",
				set_penalty,
			)
			.rpcmethod("debug_network", "debugs network graph", debug_network)
			.rpcmethod("stop_probe", "stops network probe", stop_probe)
			.subscribe("sendpay_failure", retry)
			.subscribe("sendpay_success", success)
			.subscribe("shutdown", shutdown)
			.start(state)
			.await?
	{
		plugin.join().await
	} else {
		Ok(())
	}
}
async fn debug_network(_p: Plugin<PlugState>, _v: serde_json::Value) -> Result<serde_json::Value, anyhow::Error> {
	let network_graph = _p.state().networkgraph.read_only();
	network_graph.channels().iter().for_each(|(chan_id, chan)| {
		log::info!("chan_id: {}", chan_id);
		log::info!("chan: {:?}", chan);
	});	
	Ok(json!("debugged network"))
}

async fn stop_probe(_p: Plugin<PlugState>, _v: serde_json::Value) -> Result<serde_json::Value, anyhow::Error> {
	log::info!("stopping probe");
	let ct = _p.state().ct_token.lock().unwrap();
	ct.cancel();
	Ok(json!("stopped probe"))
}

async fn shutdown(_p: Plugin<PlugState>, _v: serde_json::Value) -> Result<(), anyhow::Error> {
	log::info!("shutdown");
	let network_graph = _p.state().networkgraph.clone();
	let scorer = _p.state().scorer.clone();
	let ldk_data_dir = _p.state().ldk_data_dir.clone();
	// write to disk everything
	let mut network_file = File::create(format!("{}/network_graph", ldk_data_dir.clone()))?;
	network_graph
		.write(&mut network_file)
		.expect("failed to write netwrok graph to disk");

	let mut scorer_file = File::create(format!("{}/scorer", ldk_data_dir.clone()))?;
	scorer
		.write(&mut scorer_file)
		.expect("couldn't write scorer to disk");
	
	Ok(())
}

fn generate_groupid() -> u64 {
	let mut rng = rand::thread_rng();
	rng.gen::<u64>()
}

fn generate_partid() -> u16 {
	let mut rng = rand::thread_rng();
	rng.gen::<u16>()
}

async fn set_penalty(
	_p: Plugin<PlugState>,
	v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
	let pkey = match secp256k1::PublicKey::from_str(
		&v[0].as_str().expect("provide public key").replace("\"", ""),
	) {
		Ok(s) => s,
		Err(e) => return Err(anyhow!(e)),
	};
	let value = &v[1].as_u64().expect("no integer");
	_p.state()
		.scorer
		.lock()
		.unwrap()
		.set_manual_penalty(&NodeId::from_pubkey(&pkey), *value);
	Ok(json!("penalty was set"))
}

// This asynchronous function starts a network probe by iterating over all nodes and calling the probe function for each node.
async fn network_probe(
	_p: Plugin<PlugState>,
	_v: serde_json::Value,
) -> Result<serde_json::Value, Error> {
	match sync_graph(_p.clone()).await {
		Ok(_) => {},
		Err(_) => return Err(anyhow!("failed to sync graph")),
	};

	let graph = _p.clone().state().networkgraph.clone();
	let nodes = graph.read_only().nodes().clone();
	*_p.state().ct_token.lock().unwrap() = CancellationToken::new();
	tokio::spawn(async move {
		for key in nodes.keys() {
			let pubkey = secp256k1::PublicKey::from_slice(&key.as_slice()).unwrap();
			let _p_clone = _p.clone();
	
			probe(_p_clone.clone(), json!([pubkey.to_string()])).await.unwrap();
			if _p.state().ct_token.lock().unwrap().is_cancelled() {
				log::info!("stopped network probe");
				break;
			}
			tokio::time::sleep(Duration::from_millis(5000)).await;
		}	
	});	
	Ok(json!("network probe started"))
}

async fn success(plugin: Plugin<PlugState>, v: serde_json::Value) -> Result<(), Error> {
	let payment_guard = plugin.state().send_pay_routes.lock().unwrap();
	let scorer = plugin.state().clone().scorer;
	let id = v["sendpay_success"]["id"].to_string().replace('\"', "");
	log::debug!("{id} id of success");
	let payment = match payment_guard.get(&id){
		Some(s) => s.to_owned(),
		None => return Ok(()), // not sent with altpay
	};
	let scorer_value: Vec<&RouteHop> = payment.iter().collect();
	
	scorer
		.lock()
		.unwrap()
		.payment_path_successful(&scorer_value);
	Ok(())
}

async fn retry(plugin: Plugin<PlugState>, v: serde_json::Value) -> Result<(), Error> {
	log::info!("retrying payment");
	let id = v["sendpay_failure"]["data"]["id"]
		.to_string()
		.replace('\"', "");

	let payment ={
		let send_pay_guard = plugin.state().send_pay_routes.lock().unwrap();
		match send_pay_guard.get(&id){
			Some(s) => s.to_owned(),
			None => return Ok(()), // not sent with altpay or probe
		}
	};

	// if it is a probe payment, don't retry
	if serde_json::to_string(&v["sendpay_failure"]["data"]["payment_hash"])
		.unwrap()
		.replace('\"', "")
		== plugin.state().random_pay_hash.to_string()
	{
		log::info!("not retrying because probe");
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("Time went backwards")
			.as_millis();

		// get item in vec with value id
		let len_route = payment.len();
		let created_stamp = {
			let guard = plugin.state().payments.lock().unwrap();
			let payment = guard.get(&id).unwrap().clone();
			payment.created_at
		};
		log::info!(
			"{}ms ping time; route len of {}",
			now - created_stamp,
			len_route
		);

		return Ok(());
	}
	log::debug!("{}", "retry payment called");
	let state = plugin.state().clone();
	let scid = serde_json::to_string(&v["sendpay_failure"]["data"]["erring_channel"])
		.unwrap()
		.replace('\"', "");
	let scid = cl_to_int(&scid);

	let scorer_value: Vec<RouteHop> = {
		let guard = state.send_pay_routes.lock().unwrap();
		guard.get(&id).unwrap().clone()
	};
	let scorer_value: Vec<&RouteHop> = scorer_value.iter().collect();
	state
		.scorer
		.lock()
		.unwrap()
		.payment_path_failed(&scorer_value, scid);
	
	let scid = serde_json::to_string(&v["sendpay_failure"]["data"]["erring_channel"])
		.unwrap()
		.replace('\"', "");

	let bolt11 = &v["sendpay_failure"]["data"]["bolt11"]
		.to_string()
		.replace('\"', "");
	
	
	
	// add failed channel to payment
	{
		let mut payment_guard = plugin.state().payments.lock().unwrap();
		let mut failed_pay = payment_guard.get(bolt11).unwrap().clone();
		failed_pay.failed_channels.push(cl_to_int(&scid));
		failed_pay.retry_count += 1;
		payment_guard.insert(bolt11.to_string(), failed_pay);
	}
	let payment = {
		let payment_guard = plugin.state().payments.lock().unwrap();
		payment_guard.get(bolt11).unwrap().clone()
	};
	if payment.retry_count > 8 {
		log::info!("{} retries exceeded", payment.retry_count);
		return Ok(());
	}
	retry_pay(payment, plugin.clone()).await;

	Ok(())
}


// converts shortchannelid from c-lightning format to u64 format which ldk uses
fn cl_to_int(scid: &str) -> u64 {
	let split = scid.split('x');
	let vec: Vec<&str> = split.collect();
	vec[0].parse::<u64>().unwrap() << 40
		| vec[1].parse::<u64>().unwrap() << 16
		| vec[2].parse::<u64>().unwrap()
}

// converts shortchannelid from u64 Format which ldk uses to c-lightning format
fn u64_cl(scid: u64) -> std::string::String {
	let block = scid >> 40;
	let tx = scid >> 16 & 0xFFFFFF;
	let output = scid & 0xFFFF;

	format!("{block}x{tx}x{output}")
}
// for probing
async fn probe(
	plugin: Plugin<PlugState>,
	cli_arguments: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
	match sync_graph(plugin.clone()).await {
		Ok(_) => (),
		Err(_) => return Err(anyhow!("couldn't sync graph: ")),
	};
	let pub_key_arg = match cli_arguments[0].as_str() {
		Some(s) => s,
		None => return Ok(json!("no pub key given as first argument")),
	};
	let probe_option = cli_arguments[1].as_u64().unwrap_or(plugin.state().config.probe_amount);
	let payee_key = match secp256k1::PublicKey::from_str(pub_key_arg) {
		Ok(value) => value,
		Err(e) => return Ok(json!(e.to_string())),
	};

	let featrues = lightning::ln::features::InvoiceFeatures::from_le_bytes(vec![0, 130, 2]);
	let my_params = PaymentParameters {
		payee_pubkey: payee_key,
		features: Some(featrues),
		route_hints: Vec::new(),
		expiry_time: None,
		max_total_cltv_expiry_delta: 1008,
		max_path_count: 10,
		max_channel_saturation_power_of_half: plugin.state().config.mpp_pref, 
		previously_failed_channels: plugin.state().failed_channels.lock().unwrap().to_vec(), // TODO
	};
	let route_params = RouteParameters {
		payment_params: my_params,
		final_value_msat: probe_option, 
		final_cltv_expiry_delta: 18,
	};
	let payment_hash = plugin.state().random_pay_hash;
	let payment_secret = PaymentSecret([42u8; 32]);

	// route_params
	let route: Route = match route_find(plugin.clone(), &route_params).await {
		Ok(s) => s,
		Err(e) => {
			log::error!("{}", e.to_string());
			return Err(anyhow!("failed to find route"));
		}
	};
	let total_amount = Amount::from_msat(probe_option);
	get_and_send_route(
		route,
		payment_hash,
		payment_secret,
		String::from("Probe"),
		plugin.clone(),
		generate_groupid(),
		total_amount,
	)
	.await;
	Ok(json!("probe sent successfully"))
}
/// get route from ldk and send it to c-lightning
async fn get_and_send_route(
	route: Route,
	payment_hash: bitcoin_hashes::sha256::Hash,
	payment_secret: lightning::ln::PaymentSecret,
	string_invoice: String,
	plugin: Plugin<PlugState>,
	p_groupid: u64,
	total_amount: Amount,
) {
	let rpc_path = plugin.state().config.rpc_path.clone();
	let path_object = Path::new(&rpc_path);
	let mut rpc = ClnRpc::new(path_object).await.unwrap();

	// routes which are sent to c-lightning
	let mut routes: Vec<Vec<SendpayRoute>> = Vec::new();
	

	// route can consist of multiple paths because of multi part payments
	for ldk_path in route.paths.iter() {

		let mut subroute: Vec<SendpayRoute> = Vec::new();
		// ldk tracks in routehops the fees to pay in at the hop
		// so ate the last hop is the invoice amount which should arrive
		// https://docs.rs/lightning/latest/lightning/routing/router/struct.RouteHop.html#structfield.fee_msat
		let mut fees = 0;
		let mut cltv_total: u16 = 0;
		// total fees to be paid
		for i in 0..ldk_path.len() - 1 {
			fees += ldk_path[i].fee_msat
		}
		let amount = Amount::from_msat(ldk_path.last().unwrap().fee_msat);
		// total cltv and save route hops
		for i in 0..ldk_path.len() {
			cltv_total += u16::try_from(ldk_path[i].cltv_expiry_delta).unwrap()
		}
		for j in 0..ldk_path.len() - 1 {
			let path: &RouteHop = &ldk_path[j];
			let delay = if j == 0 {
				cltv_total
			} else {
				u16::try_from(subroute[j - 1].delay).expect("failed u16")
					- u16::try_from(ldk_path[j - 1].cltv_expiry_delta)
						.expect("delay couldn't be converted to u16")
			};
			let i = SendpayRoute {
				// amount which is expected at this hop equals to payment amount plus the current fees at his hop
				// same goes for cltv
				amount_msat: Amount::from_msat(fees) + amount,
				id: path.pubkey,
				delay,
				channel: ShortChannelId::from_str(&u64_cl(path.short_channel_id))
					.expect("couldn't convert from u64 to scid"),
			};
			fees -= path.fee_msat;
			subroute.push(i)
		}
		// set last hop manually because
		// if path consists of only one routehop special treatment
		if ldk_path.len() == 1 {
			let i = SendpayRoute {
				amount_msat: Amount::from_msat(ldk_path.last().unwrap().fee_msat),
				id: ldk_path.last().unwrap().pubkey,
				delay: u16::try_from(ldk_path.last().unwrap().cltv_expiry_delta).unwrap(),
				channel: ShortChannelId::from_str(&u64_cl(ldk_path.last().unwrap().short_channel_id))
					.expect("couldn't convert from u64 to scid"),
			};
			subroute.push(i);
			routes.push(subroute);
		} else {
			let i = SendpayRoute {
				amount_msat: Amount::from_msat(ldk_path.last().unwrap().fee_msat),
				id: ldk_path.last().unwrap().pubkey,
				// https://docs.rs/lightning/0.0.113/lightning/routing/router/struct.RouteHop.html#structfield.cltv_expiry_delta
				// The CLTV delta added for this hop. For the last hop, this should be the full CLTV value expected at the destination, in excess of the current block height.
				delay: u16::try_from(subroute.last().unwrap().delay).expect("failed u16")
					- u16::try_from(ldk_path[ldk_path.len() - 2].cltv_expiry_delta)
						.expect("delay couldn't be converted to u16"),
				channel: ShortChannelId::from_str(&u64_cl(ldk_path.last().unwrap().short_channel_id))
					.expect("couldn't convert from u64 to scid"),
			};
			subroute.push(i);
			routes.push(subroute);
		}
	}


	// send the routes to c-lightning
	for i in 0..routes.len() {
		let partid = Some(generate_partid());

		let my_request = SendpayRequest {
			route: routes[i].to_vec(),
			payment_hash,
			label: None,
			amount_msat: Some(total_amount), //final amount is equal to amount expected at last hop
			bolt11: Some(string_invoice.clone()),
			payment_secret: Some(Secret::try_from(payment_secret.0.to_vec()).unwrap()),
			partid: partid, 
			localinvreqid: None,
			groupid: Some(p_groupid),
		};

		match rpc.call(Request::SendPay(my_request)).await {
			Ok(p) => {
				log::info!("sendpay succeeded: {:?}", p);
				let sent_payment = route.paths[i].clone();
				let tmp: serde_json::Value =
					serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
				let id = tmp["result"]["id"].to_string().replace('\"', "");
				plugin.state()
					.clone()
					.send_pay_routes
					.lock()
					.unwrap()
					.insert(id, sent_payment);
			}
			Err(s) => {
				log::error!("sendpay failed: {}", s);
				log::error!("sendpay failed: {}", s);
			}
		};
	}
}

async fn sync_graph(plugin: Plugin<PlugState>) ->Result<(), ()> {
	let ldk_data_dir = plugin.state().ldk_data_dir.clone();
	let network_graph = plugin.state().networkgraph.clone();

	// sync graph with rapid sync
	let rapid_sync = RapidGossipSync::new(network_graph.clone());
	let timestam: u64 = network_graph
		.get_last_rapid_gossip_sync_timestamp()
		.unwrap_or(0).try_into().unwrap();

	if SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap()
		.as_secs()
		.saturating_sub(timestam)
		< 60 * 5 // syncs every 5 minutes
		&& timestam != 0
	{
		log::debug!("no need to sync graph");
		Ok(())
	} else {
		log::debug!("syncing graph");
		let url = plugin.state().config.RapidGossipSync_URL.clone();
	
		let target = format!("{url}{timestam}");
		let response = reqwest::get(target.clone()).await.unwrap().bytes().await.unwrap();
		let mut out = File::create(format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())).unwrap();
		out.write_all(&response).unwrap();
		match rapid_sync
			.sync_network_graph_with_file_path(&format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())){
			Ok(_) => {
				log::debug!("synced graph");
				return Ok(());
			}
			Err(e) => {
				log::error!("error syncing graph: {:?}", e);
				log::error!("deleting graph and trying again");
				fs::remove_file(format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())).unwrap();
				let target = format!("{url}0");
				let response = reqwest::get(target).await.unwrap().bytes().await.unwrap();
				let mut out = File::create(format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())).unwrap();
				out.write_all(&response).unwrap();
				// trying again to sync after deleting graph
				match rapid_sync.sync_network_graph_with_file_path(&format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())){
					Ok(_) => {
						log::debug!("synced graph");
						return Ok(());
					}
					Err(e) => {
						log::error!("error syncing graph: {:?}", e);
						return Err(());
					}
				}
				
			}
		
	}}

}

async fn route_find(plugin: Plugin<PlugState>, route_params: &RouteParameters) -> Result<Route, anyhow::Error> {
	match sync_graph(plugin.clone()).await {
		Ok(_) => (),
		Err(_) => return Err(anyhow!("error syncing graph")),
	}; 

	let mut bytes = [0u8; 32];
	OsRng.fill_bytes(&mut bytes);

	// trying to get first hops
	let rpc_path = plugin.state().config.rpc_path.clone();
	let p = Path::new(&rpc_path);
	let mut rpc = ClnRpc::new(&p).await.unwrap();
	let listpeer_response = rpc
		.call(Request::ListPeers( ListpeersRequest{ id: None, level: None }))
		.await
		.unwrap();
	let listpeer_response: ListpeersResponse = ListpeersResponse::try_from(listpeer_response).unwrap();
	//let response: ListfundsResponse = ListfundsResponse::try_from(response).unwrap();
	let mut first_hops = Vec::new();
	let network_graph = plugin.state().networkgraph.read_only();
	for peer in listpeer_response.peers{
		if peer.connected{
			for channel in peer.channels{
				match channel.state {
			        cln_rpc::model::ListpeersPeersChannelsState::CHANNELD_NORMAL => {
						log::debug!("spendable msat: {} for channel {:?}", channel.spendable_msat.unwrap().msat(), channel.short_channel_id.unwrap());
						log::info!("channel id: {:?}", channel.short_channel_id);
						let local_channel_state = network_graph.channel(cl_to_int(&channel.short_channel_id.unwrap().to_string()));
						let features = match local_channel_state {
							Some(local_channel_state) => local_channel_state.features.encode(),
							None => {
								log::error!("channel {:?} not found in local graph", channel.short_channel_id.unwrap());
								continue;
							}
						};
						log::info!("features: {:?}", features);
						// spendable msat is the pretty much the only interesting info we need
						// rest is useless for routing purposes
						let hop = ChannelDetails {
							channel_id: [2; 32],
							counterparty: ChannelCounterparty{ node_id: peer.id, features : InitFeatures::from_le_bytes(features) , unspendable_punishment_reserve: 0, forwarding_info: None, outbound_htlc_minimum_msat: None, outbound_htlc_maximum_msat: None },
							funding_txo: None,
							channel_type: None,
							short_channel_id: Some(cl_to_int(&channel.short_channel_id.unwrap().to_string())),
							outbound_scid_alias: None,
							inbound_scid_alias: None,
							channel_value_satoshis: channel.total_msat.unwrap().msat() / 1000,
							unspendable_punishment_reserve: Some(channel.our_reserve_msat.unwrap().msat() / 1000),
							user_channel_id: 0,
							balance_msat: channel.spendable_msat.unwrap().msat(),
							outbound_capacity_msat: channel.spendable_msat.unwrap().msat(),
							next_outbound_htlc_limit_msat: channel.maximum_htlc_out_msat.unwrap().msat(),
							inbound_capacity_msat: 0,
							confirmations_required: Some(3),
							confirmations: Some(100),
							force_close_spend_delay: Some(100),
							is_outbound: true,
							is_channel_ready: true,
							is_usable: true,
							is_public: true,
							inbound_htlc_minimum_msat: None,
							inbound_htlc_maximum_msat: None,
							config: None,
    					};
						first_hops.push(hop);
					},
					_ => (),
    			}
			}
		}
	}

	let router = DefaultRouter::new(
		plugin.state().networkgraph.clone(),
		plugin.state().logger.clone(),
		bytes,
		plugin.state().scorer.clone(),
	);
	let channels = first_hops.iter().collect::<Vec<&ChannelDetails>>();
	let channel_refs = Some(channels.as_slice());

	match router.find_route(&plugin.state().pk, route_params, channel_refs, InFlightHtlcs::new()) {
		Ok(s) => {
			log::info!("found route: {:?}", s.paths);
			//panic!("testing");
			return Ok(s);
		},
		Err(e) => {
			log::error!("{:?}", e);
			Err(anyhow!("error finding route"))
		}
	}
}

async fn retry_pay(failed_pay: Payment, plugin: Plugin<PlugState>){
	let invoice = failed_pay.bolt11.parse::<Invoice>().unwrap();
	let route_params = route_params_from_invoice(invoice.clone(), plugin.clone(), failed_pay.failed_channels).await.unwrap();
	let route = route_find(plugin.clone(), &route_params).await.unwrap();
	let payment_hash = *invoice.payment_hash();
	let payment_secret = *invoice.payment_secret();
	get_and_send_route(route, payment_hash, payment_secret, failed_pay.bolt11, plugin, failed_pay.groupid, failed_pay.total_amount).await;
}

async fn route_params_from_invoice(invoice: Invoice, plugin: Plugin<PlugState>, failed_channels: Vec<u64>) -> Result<RouteParameters, Error> {
	// get data from invoice
	let hint = invoice.route_hints();
	let payee_key = invoice.payee_pub_key();
	let invoice_feat = invoice.features();
	let expiry = Some(invoice.expiry_time().as_secs()); // returns in seconds expiry time
	let payee_key = if payee_key.is_none() {
		invoice.recover_payee_pub_key()
	} else {
		*payee_key.unwrap()
	};
	if invoice.is_expired() {
		log::info!("invoice is expired");
		return Err(anyhow!("invoice is expired"));
	}
	// need expiry time in seconds from UNIX_EPOCH for PaymnetParameters
	let expiry = match expiry {
		Some(expiry) => Some(
			expiry
				+ invoice
					.timestamp()
					.duration_since(UNIX_EPOCH)
					.unwrap()
					.as_secs(),
		),
		None => {
			log::debug!("invoice has no expiry");
			None
		}
	};
	// create PaymentParameters to pass into router
	let my_params = PaymentParameters {
		payee_pubkey: payee_key,
		features: invoice_feat.cloned(),
		route_hints: hint,
		expiry_time: expiry,
		max_total_cltv_expiry_delta: 1008,
		max_path_count: 10,
		max_channel_saturation_power_of_half: plugin.state().config.mpp_pref,
		previously_failed_channels: failed_channels,
	};
	// RouteParameters to pass into router
	let route_params = RouteParameters {
		payment_params: my_params,
		final_value_msat: invoice.amount_milli_satoshis().unwrap(),
		final_cltv_expiry_delta: 18,
	};
	Ok(route_params)
}

async fn altpay_method(
	plugin: Plugin<PlugState>,
	cli_arguments: serde_json::Value,
) -> Result<serde_json::Value, Error> {
	let ldk_data_dir = plugin.state().ldk_data_dir.clone();
	let network_graph = plugin.state().networkgraph.clone();
	let scorer = plugin.state().scorer.clone();

	// invoice stuff
	let string_invoice = match cli_arguments.get(0) {
		Some(invoice) => invoice.as_str().unwrap().replace('\\', ""),
		None => {
			log::error!("no invoice provided");
			return Ok(json!("no invoice provided"));
		}
	};
	let invoice = match string_invoice.parse::<SignedRawInvoice>(){
		Ok(invoice) => Invoice::from_signed(invoice).unwrap(),
		Err(e) => {
			log::error!("error parsing invoice: {:?}", e);
			return Ok(json!("error parsing invoice"));
			},
	};
	
	let route_params = match route_params_from_invoice(invoice.clone(), plugin.clone(), vec![]).await {
		Ok(value) => value,
		Err(e) => return Ok(json!(e.to_string())),
	};


	let route: Route = match route_find(plugin.clone(), &route_params).await {
		Ok(value) => value,
		Err(e) => return Ok(json!(e.to_string())),
	};

	let payment_hash = *invoice.payment_hash();
	let payment_secret = invoice.payment_secret();
	let total_amount = Amount::from_msat(route.get_total_amount());

	let groupid = generate_groupid();
	plugin.state().payments.lock().unwrap().insert(string_invoice.clone(), Payment { total_amount: total_amount, failed_channels: vec![], created_at: SystemTime::now().duration_since(UNIX_EPOCH).expect("time went backwards").as_millis(), retry_count: 0, groupid: groupid, payment_secret: *payment_secret, bolt11: string_invoice.clone() });


	get_and_send_route(
		route,
		payment_hash,
		*payment_secret,
		string_invoice.clone(),
		plugin.clone(),
		groupid,
		total_amount,
	)
	.await;

	// write to disk everything
	tokio::spawn(async move {
		let mut network_file = File::create(format!("{}/network_graph", ldk_data_dir.clone())).unwrap();
		network_graph
			.write(&mut network_file)
			.expect("failed to write netwrok graph to disk");

		let mut scorer_file = File::create(format!("{}/scorer", ldk_data_dir.clone())).unwrap();
		scorer
			.write(&mut scorer_file)
			.expect("couldn't write scorer to disk");
		log::info!("writing to disk successful");
	});
	Ok(json!("altpay sent, payment can still fail"))

}
