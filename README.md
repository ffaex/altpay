# Altpay - Alternative payment Plugin for c-lightning

Altpay is a Lightning Network routing plugin for c-lightning that provides alternative routing using probabilistic scoring. It improves payment success rates and reduces payment fees by finding the most optimal routes in the network.
## Features

- Optimized routing using probabilistic scoring
- Penalty setting for specific nodes
- Learns from successul payments and failed payments more about channel liquidities, those bounds get relaxed over time
- Pickhardt payments
- set preference for MPP in settings file

## Requirements

- Rust (see https://www.rust-lang.org/tools/install)
- sudo apt install build-essential
- sudo apt install libssl-dev
- sudo apt install pkg-config


## Installation

1. Clone the repository:
git clone https://github.com/ffaex/altpay.git

2. Change to the `altpay` directory:
cd altpay

3. Build the project:
cargo build --release

4. Copy the generated binary to a desired location:
cp target/release/altpay /usr/local/bin/

## Configuration

1. Create a configuration file named `altpay.toml` in $HOME/.config/. Set the appropriate values:

network = "testnet" # Use "bitcoin" for mainnet  
ldk_data_dir = "/path/to/your/ldk_data_dir"  
rpc_path = "/path/to/your/lightning-rpc"  
mpp_pref = 0 (the lower the better)  
rpc_path="/root/.lightning/testnet/lightning-rpc" # path to the clightning rpc socket for mainnet usually /root/.lightning/bitcoin/lightning-rpc  
rapid_gossip_sync_url = "http://rapidsync.fyodor.de/mainnet/snapshot/"

2. set PK env variable to your public key of your lighting node 
- echo 'export PK="your public key"' >> $HOME/.bashrc
- source $HOME/.bashrc
4. Start lightningd --testnet --plugin=/path to binary

Please note if opening new channels it can take up to 30min for the plugin to see them. Because it uses RapidSync https://lightningdevkit.org/blog/announcing-rapid-gossip-sync/. Uses this rapidsync server http://rapidsync.fyodor.de/. SNAPSHOT_CALCULATION_INTERVAL set to 30min
# Usage 
lightning-cli --testnet altpay bolt11_invoice  
lightning-cli --testnet probe pubkey amount_msat  
lightning-cli --testnet set_penalty pubkey penalty_value (the lower the better)
lightning-cli --testnet network_probe 

# Special Thanks  
[@elsirion](https://github.com/elsirion)  
[@fiatjaf](https://github.com/fiatjaf)
