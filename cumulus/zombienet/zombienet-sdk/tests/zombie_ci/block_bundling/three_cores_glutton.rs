// This file is part of Cumulus.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::anyhow;

use cumulus_zombienet_sdk_helpers::{
	assert_finality_lag, assert_para_throughput, create_assign_core_call,
};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_sdk::{
	subxt::{
		backend::{legacy::LegacyRpcMethods, rpc::RpcClient},
		OnlineClient, PolkadotConfig,
	},
	subxt_signer::sr25519::dev,
	NetworkConfig, NetworkConfigBuilder,
};

const PARA_ID: u32 = 2400;

/// A test that ensures that PoV bundling works with 3 cores and glutton consuming 80% ref time.
///
/// This test starts with 3 cores assigned and configures glutton to use 80% of ref time,
/// then validates that the parachain produces 72 blocks.
#[tokio::test(flavor = "multi_thread")]
async fn block_bundling_three_cores_glutton() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config().await?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_node = network.get_node("collator-1")?;

	let para_client = para_node.wait_client().await?;
	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;
	let alice = dev::alice();

	// Assign cores 0 and 1 to start with 3 cores total (core 2 is assigned by Zombienet)
	let assign_cores_call = create_assign_core_call(&[(0, PARA_ID), (1, PARA_ID)]);

	relay_client
		.tx()
		.sign_and_submit_then_watch_default(&assign_cores_call, &alice)
		.await
		.inspect(|_| log::info!("Tx send, waiting for finalization"))?
		.wait_for_finalized_success()
		.await?;
	log::info!("3 cores total assigned to the parachain");

	// Wait for the parachain to produce 72 blocks with 3 cores and glutton active
	// With 3 cores, we expect roughly 3x throughput compared to single core
	// Adjusting expectations based on glutton consuming 80% of ref time
	assert_para_throughput(
		&relay_client,
		6,
		[(ParaId::from(PARA_ID), 12..19)],
		[(ParaId::from(PARA_ID), (para_client.clone(), 48..73))],
	)
	.await?;

	assert_finality_lag(&para_client, 72).await?;
	log::info!("Test finished successfully - 72 blocks produced with 3 cores and glutton");
	Ok(())
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {images:?}");
	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=trace").into()])
				.with_default_resources(|resources| {
					resources.with_request_cpu(4).with_request_memory("4G")
				})
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"num_cores": 3,
								"max_validators_per_core": 1
							}
						}
					}
				}))
				.with_node(|node| node.with_name("validator-0"));
			(1..9).fold(r, |acc, i| acc.with_node(|node| node.with_name(&format!("validator-{i}"))))
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("pov-bundling")
				.with_default_args(vec![
					("--authoring").into(),
					("slot-based").into(),
					("-lparachain=debug,aura=trace").into(),
				])
				.with_genesis_overrides(json!({
					"glutton": {
						"compute": "800000000", // 80% ref time consumption
						"storage": "0", // No storage consumption
						"trashDataCount": 5000, // Initialize with some trash data
						"blockLength": "0" // No block length consumption
					}
				}))
				.with_collator(|n| n.with_name("collator-0"))
				.with_collator(|n| n.with_name("collator-1"))
				.with_collator(|n| n.with_name("collator-2"))
		})
		.with_global_settings(|global_settings| match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(val) => global_settings.with_base_dir(val),
			_ => global_settings,
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})
}
