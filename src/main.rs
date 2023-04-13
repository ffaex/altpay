#[macro_use]
extern crate serde_json;
mod disk;
use crate::disk::FilesystemLogger;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1;
use cln_plugin::{Builder, Error, Plugin};
use rand::Rng;
use cln_rpc::model::{SendpayRequest, SendpayRoute};
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
use lightning::ln::PaymentSecret;
use config::Config;
use lightning::routing::gossip::{NetworkGraph, NodeId};
use lightning_rapid_gossip_sync::RapidGossipSync;
use rand_core::{OsRng, RngCore};
use tokio_util::sync::CancellationToken;

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
	vec_routes: Arc<Mutex<HashMap<String, (u128, Box<[RouteHop]>)>>>,
	pk: secp256k1::PublicKey,
	random_pay_hash: sha256::Hash,
	config: Conf,
	ct_token: Arc<Mutex<CancellationToken>>,
}

#[derive(Debug, Deserialize, Clone)]
struct Conf {
	network: String,
	ldk_data_dir: String,
	rpc_path: String,
	mpp_pref: u8,
	probe_amount: u64,
}
#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
	let home_dir = match env::var("HOME") {
        Ok(value) => value,
        Err(_) => {
            log::info!("HOME not set");
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
		.set_default("network", "testnet")?
		.set_default("ldk_data_dir", format!("{}/.lightning/.ldk/", home_dir))?
		.set_default("rpc_path", format!("{}/.lightning/testnet/lightning-rpc", home_dir))?
		.set_default("mpp_pref", 0)?
		.set_default("probe_amount", 1000)?
		.add_source(config::File::with_name(&format!("{}/.config/altpay.toml", home_dir)))
		.build()
		.unwrap();
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


	let random_pay_hash = sha256::Hash::from_slice(&random_bytes[..]).unwrap();

	let vec_routes: Arc<Mutex<HashMap<String, (u128, Box<[RouteHop]>)>>> =
		Arc::new(Mutex::new(HashMap::new()));
	let state = PlugState {
		networkgraph: network_graph,
		scorer,
		ldk_data_dir,
		logger,
		failed_channels,
		vec_routes,
		pk: secp256k1::PublicKey::from_str(&pk).unwrap(),
		random_pay_hash,
		config,
		ct_token: Arc::new(Mutex::new(CancellationToken::new())),
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
		// sync_graph(plugin.clone()).await;
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
	sync_graph(_p.clone()).await;

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
	let vec_routes = plugin.state().clone().vec_routes;
	let scorer = plugin.state().clone().scorer;
	let id = v["sendpay_success"]["id"].to_string().replace('\"', "");
	log::debug!("{id} id of success");
	let route_hops = match vec_routes.lock().unwrap().get(&id){
		Some(s) => s.to_owned().1,
		None => return Ok(()), // not sent with altpay
	};
	let mut scorer_value: Vec<_> = Vec::new();
	for i in route_hops.iter() {
		scorer_value.push(i)
	}

	scorer
		.lock()
		.unwrap()
		.payment_path_successful(&scorer_value);
	Ok(())
}

async fn retry(plugin: Plugin<PlugState>, v: serde_json::Value) -> Result<(), Error> {
	let id = v["sendpay_failure"]["data"]["id"]
		.to_string()
		.replace('\"', "");

	if serde_json::to_string(&v["sendpay_failure"]["data"]["payment_hash"])
		.unwrap()
		.replace('\"', "")
		== plugin.state().random_pay_hash.to_string()
	{
		log::info!("not retrying because probe");
		let vec_routes = plugin.state().clone().vec_routes;
		let routes_guard = vec_routes.lock().unwrap();
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("Time went backwards")
			.as_millis();

		let created_stamp = routes_guard.get(&id).expect("id not found").0;
		let len_route = routes_guard.get(&id).expect("id not found").1.len();
		log::info!(
			"{}ms ping time; route len of {}",
			now - created_stamp,
			len_route
		);

		return Ok(());
	}
	log::debug!("{}", "retry payment called");
	let bolt11 = json!([&v["sendpay_failure"]["data"]["bolt11"]]);
	let state = plugin.state().clone();
	let scid = serde_json::to_string(&v["sendpay_failure"]["data"]["erring_channel"])
		.unwrap()
		.replace('\"', "");
	let scid = cl_to_int(&scid);

	let vec_routes = plugin.state().clone().vec_routes;
	let route_hops = vec_routes.lock().unwrap().get(&id).unwrap().to_owned().1;

	let mut scorer_value: Vec<_> = Vec::new();
	for i in route_hops.iter() {
		scorer_value.push(i)
	}

	{
		let guard = state.vec_routes.lock().unwrap();
		let hashmap_result = guard.get(&id);
		if hashmap_result.is_some() {
			state
				.scorer
				.lock()
				.unwrap()
				.payment_path_failed(&scorer_value, scid);
		}
	}
	altpay_method(plugin.clone(), bolt11.clone()).await?;

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
	sync_graph(plugin.clone()).await;

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
		max_channel_saturation_power_of_half: plugin.state().config.mpp_pref, // might be used to optimize stuff lol TODO
		previously_failed_channels: plugin.state().failed_channels.lock().unwrap().to_vec(), // TODO
	};
	// log::info!("{:?}", my_params);
	let route_params = RouteParameters {
		payment_params: my_params,
		final_value_msat: probe_option, //_p.state().conf.probe_amount,
		final_cltv_expiry_delta: 18,
	};
	let payment_hash = plugin.state().random_pay_hash;
	let payment_secret = PaymentSecret([42u8; 32]);

	// route_params
	let route: Route = match route_find(plugin.clone(), &route_params).await {
		Some(s) => s,
		None => {
			log::error!("failed to find route");
			plugin.state().failed_channels.lock().unwrap().clear();
			return Ok(json!("failed to find route"));
		}
	};
	get_and_send_route(
		route,
		payment_hash,
		payment_secret,
		Amount::from_msat(probe_option),
		String::from("Probe"),
		plugin.clone(),
	)
	.await;
	Ok(json!("probe sent successfully"))
}
/// get route from ldk and send it to c-lightning
async fn get_and_send_route(
	route: Route,
	payment_hash: bitcoin_hashes::sha256::Hash,
	payment_secret: lightning::ln::PaymentSecret,
	amount: cln_rpc::primitives::Amount,
	string_invoice: String,
	plugin: Plugin<PlugState>,
) {
	let rpc_path = plugin.state().config.rpc_path.clone();
	let path_object = Path::new(&rpc_path);
	let mut rpc = ClnRpc::new(path_object).await.unwrap();

	let mut routes = Vec::new();
	
	// amount to pay to payee
	let amount = amount;

	let mut save_routes: Vec<Box<[RouteHop]>> = Vec::new();
	// route can consist of multiple paths because of multi part payments
	for paths in route.paths.iter() {
		let mut save_route: Vec<RouteHop> = Vec::new();
		for i in paths.clone() {
			save_route.push(i)
		}
		save_routes.push(save_route.into_boxed_slice());
		let mut subroute: Vec<SendpayRoute> = Vec::new();
		// ldk tracks in routehops the fees to pay in at the hop
		// so ate the last hop is the invoice amount which should arrive
		// log::info!("{:?}", paths);
		let mut fees = 0;
		let mut cltv_total: u16 = 0;
		// total fees to be paid
		for i in 0..paths.len() - 1 {
			fees += paths[i].fee_msat
		}
		// total cltv and save route hops
		for i in 0..paths.len() {
			cltv_total += u16::try_from(paths[i].cltv_expiry_delta).unwrap()
		}
		for j in 0..paths.len() - 1 {
			let path: &RouteHop = &paths[j];
			let delay = if j == 0 {
				cltv_total
			} else {
				u16::try_from(subroute[j - 1].delay).expect("failed u16")
					- u16::try_from(paths[j - 1].cltv_expiry_delta)
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
		if paths.len() == 1 {
			let i = SendpayRoute {
				amount_msat: Amount::from_msat(paths.last().unwrap().fee_msat),
				id: paths.last().unwrap().pubkey,
				delay: u16::try_from(paths.last().unwrap().cltv_expiry_delta).unwrap(),
				channel: ShortChannelId::from_str(&u64_cl(paths.last().unwrap().short_channel_id))
					.expect("couldn't convert from u64 to scid"),
			};
			subroute.push(i);
			routes.push(subroute);
		} else {
			let i = SendpayRoute {
				amount_msat: Amount::from_msat(paths.last().unwrap().fee_msat),
				id: paths.last().unwrap().pubkey,
				// https://docs.rs/lightning/0.0.113/lightning/routing/router/struct.RouteHop.html#structfield.cltv_expiry_delta
				// The CLTV delta added for this hop. For the last hop, this should be the full CLTV value expected at the destination, in excess of the current block height.
				delay: u16::try_from(subroute.last().unwrap().delay).expect("failed u16")
					- u16::try_from(paths[paths.len() - 2].cltv_expiry_delta)
						.expect("delay couldn't be converted to u16"),
				channel: ShortChannelId::from_str(&u64_cl(paths.last().unwrap().short_channel_id))
					.expect("couldn't convert from u64 to scid"),
			};
			subroute.push(i);
			routes.push(subroute);
		}
	}


	// create groupid if its a MPP payment
	let groupid = if routes.len() > 1 {
		Some(generate_groupid())
	} else {
		None
	};

	for i in 0..routes.len() {
		let my_request = SendpayRequest {
			route: routes[i].to_vec(),
			payment_hash,
			label: None,
			amount_msat: Some(routes[i].to_vec().last().unwrap().amount_msat), //final amount is equal to amount expected at last hop
			bolt11: Some(string_invoice.clone()),
			payment_secret: Some(Secret::try_from(payment_secret.0.to_vec()).unwrap()),
			partid: Some(i as u16), 
			localinvreqid: None,
			groupid: groupid,
		};

		match rpc.call(Request::SendPay(my_request)).await {
			Ok(p) => {
				let start_time = SystemTime::now()
					.duration_since(UNIX_EPOCH)
					.expect("Time went backwards")
					.as_millis();
				let tmp: serde_json::Value =
					serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
				let id = tmp["result"]["id"].to_string().replace('\"', "");
				log::debug!("{id} id of insert");
				plugin.state()
					.clone()
					.vec_routes
					.clone()
					.lock()
					.unwrap()
					.insert(id, (start_time, save_routes[i].to_owned())); //TODO
			}
			Err(s) => {
				// if rpc error usually because network_graph doesnt have local state so remove chanell temporarily
				// see https://github.com/ElementsProject/lightning/issues/956
				// routing does not pay attention to the state of the local channels
				log::error!("{:?}", s);
				if s.code.unwrap() != 204 {
					log::error!("error in sendpay: {:?}", s);
				}
				plugin.state()
					.failed_channels
					.lock()
					.unwrap()
					.push(cl_to_int(&routes[i].to_vec()[0].channel.to_string()));
			}
		};
	}
}

async fn sync_graph(plugin: Plugin<PlugState>) {
	let ldk_data_dir = plugin.state().ldk_data_dir.clone();
	let network_graph = plugin.state().networkgraph.clone();

	// sync graph with rapid sync
	let rapid_sync = RapidGossipSync::new(network_graph.clone());
	let timestam: u64 = network_graph
		.get_last_rapid_gossip_sync_timestamp()
		.unwrap_or(0).try_into().unwrap();

	// if timestamp difference to now is less than 6 hours, return no reason to sync
	if SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap()
		.as_secs()
		.saturating_sub(timestam)
		< 60 * 5 // syncs every 5 minutes
		&& timestam != 0
	{
		log::debug!("no need to sync graph");
	} else {
		log::debug!("syncing graph");
		let url = if plugin.state().config.network == "bitcoin" {
			"http://rapidsync.fyodor.de/mainnet/snapshot/".to_string()
			//"https://rapidsync.lightningdevkit.org/snapshot/".to_string()
		} else {
			"http://rapidsync.fyodor.de/snapshot/".to_string()
		};
	
		let target = format!("{url}{timestam}");
		let response = reqwest::get(target.clone()).await.unwrap().bytes().await.unwrap();
		let mut out = File::create(format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())).unwrap();
		out.write_all(&response).unwrap();
		match rapid_sync
			.sync_network_graph_with_file_path(&format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())){
			Ok(_) => {
				log::debug!("synced graph");
			}
			Err(e) => {
				log::error!("error syncing graph: {:?}", e);
				fs::remove_file(format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())).unwrap();
				let target = format!("{url}0");
				let response = reqwest::get(target).await.unwrap().bytes().await.unwrap();
				let mut out = File::create(format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())).unwrap();
				out.write_all(&response).unwrap();
				rapid_sync.sync_network_graph_with_file_path(&format!("{}/rapid_sync.lngossip", ldk_data_dir.clone())).unwrap();
			}
	}}

}

async fn route_find(p: Plugin<PlugState>, route_params: &RouteParameters) -> Option<Route> {
	sync_graph(p.clone()).await; //TODO enable this

	let mut bytes = [0u8; 32];
	OsRng.fill_bytes(&mut bytes);
	let router = DefaultRouter::new(
		p.state().networkgraph.clone(),
		p.state().logger.clone(),
		bytes,
		p.state().scorer.clone(),
	);
	match router.find_route(&p.state().pk, route_params, None, InFlightHtlcs::new()) {
		Ok(s) => Some(s),
		Err(e) => {
			log::error!("{:?}", e);
			None
		}
	}
}

async fn altpay_method(
	plugin: Plugin<PlugState>,
	cli_arguments: serde_json::Value,
) -> Result<serde_json::Value, Error> {
	let ldk_data_dir = plugin.state().ldk_data_dir.clone();
	let network_graph = plugin.state().networkgraph.clone();
	let scorer = plugin.state().scorer.clone();
	let _logger = plugin.state().logger.clone();

	// invoice stuff
	let string_invoice = cli_arguments.get(0).unwrap().as_str().unwrap().replace('\\', "");
	let invoice = string_invoice.parse::<SignedRawInvoice>().unwrap();
	let invoice = Invoice::from_signed(invoice)?;

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
		return Ok(json!("invoice is expired"));
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
		previously_failed_channels: plugin.state().failed_channels.lock().unwrap().to_vec(), // TODO
	};
	// RouteParameters to pass into router
	let route_params = RouteParameters {
		payment_params: my_params,
		final_value_msat: invoice.amount_milli_satoshis().unwrap(),
		final_cltv_expiry_delta: 18,
	};

	let route: Route = match route_find(plugin.clone(), &route_params).await {
		Some(value) => value,
		None => return Ok(json!("failed at finding route")),
	};

	let payment_hash = *invoice.payment_hash();
	let payment_secret = invoice.payment_secret(); // maybe derefernce
	let amount = Amount::from_msat(invoice.amount_milli_satoshis().unwrap());

	get_and_send_route(
		route,
		payment_hash,
		*payment_secret,
		amount,
		string_invoice.clone(),
		plugin.clone(),
	)
	.await;

	// write to disk everything
	let mut network_file = File::create(format!("{}/network_graph", ldk_data_dir.clone()))?;
	network_graph
		.write(&mut network_file)
		.expect("failed to write netwrok graph to disk");

	let mut scorer_file = File::create(format!("{}/scorer", ldk_data_dir.clone()))?;
	scorer
		.write(&mut scorer_file)
		.expect("couldn't write scorer to disk");
	log::info!("altpay successful");
	Ok(json!("payment sent"))
}
