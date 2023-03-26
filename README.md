# Altpay - Alternative payment Plugin for c-lightning

Altpay is a Lightning Network routing plugin for c-lightning that provides alternative routing using probabilistic scoring. It improves payment success rates and reduces payment fees by finding the most optimal routes in the network.
## Features

- Optimized routing using probabilistic scoring
- Penalty setting for specific nodes
- Learns from successul payments and failed payments more about channel liquidities, those bounds get relaxed over time
- Pickhardt payments
- set preference for MPP in settings file

## Requirements

- Rust

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

1. Create a configuration file named `altpay.toml` in the desired location (e.g., `/etc/altpay/altpay.toml`). Set the appropriate values for your LND node:

network = "testnet" # Use "bitcoin" for mainnet
ldk_data_dir = "/path/to/your/ldk_data_dir"
rpc_path = "/path/to/your/lightning-rpc"

2. set PK env variable to your public key of your lighting node
3. Start lightningd --testnet --plugin=/path to binary

# Usage 
lightning-cli --testnet altpay bolt11_invoice  
lightning-cli --testnet probe pubkey amount_msat  
lightning-cli --testnet set_penalty pubkey penalty_value (the lower the better)
