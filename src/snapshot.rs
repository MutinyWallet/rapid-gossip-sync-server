use std::collections::HashMap;
use std::fs;
use std::ops::Deref;
use std::os::unix::fs::symlink;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use lightning::{log_info, log_error};

use lightning::routing::gossip::NetworkGraph;
use lightning::util::logger::Logger;

use crate::config;
use crate::config::cache_path;

pub(crate) struct Snapshotter<L: Deref + Clone> where L::Target: Logger {
	network_graph: Arc<NetworkGraph<L>>,
	logger: L,
}

impl<L: Deref + Clone> Snapshotter<L> where L::Target: Logger {
	pub fn new(network_graph: Arc<NetworkGraph<L>>, logger: L) -> Self {
		Self { network_graph, logger }
	}

	pub(crate) async fn snapshot_gossip(&self) {
		log_info!(self.logger, "Initiating snapshotting service");

		let snapshot_interval = config::snapshot_generation_interval() as u64;
		let mut snapshot_scopes = vec![];
		{ // double the coefficient until it reaches the maximum (limited) snapshot scope
			let mut current_scope = snapshot_interval;
			loop {
				snapshot_scopes.push(current_scope);
				if current_scope >= config::MAX_SNAPSHOT_SCOPE as u64 {
					snapshot_scopes.push(u64::MAX);
					break;
				}

				// double the current factor
				current_scope <<= 1;
			}
		}

		// this is gonna be a never-ending background job
		loop {
			self.generate_snapshots(config::SYMLINK_GRANULARITY_INTERVAL as u64, snapshot_interval, &snapshot_scopes, &cache_path(), None).await;

			// constructing the snapshots may have taken a while
			let current_time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

			// NOTE: we're waiting until the next multiple of snapshot_interval
			// however, if the symlink granularity is lower, then during that time, no intermediate
			// symlinks will be generated. That should be ok, because any timestamps previously
			// returned would already have generated symlinks, but this does have bug potential
			let remainder = current_time % snapshot_interval;
			let time_until_next_generation = snapshot_interval - remainder;

			log_info!(self.logger, "Sleeping until next snapshot capture: {}s", time_until_next_generation);
			// add in an extra five seconds to assure the rounding down works correctly
			let sleep = tokio::time::sleep(Duration::from_secs(time_until_next_generation + 5));
			sleep.await;
		}
	}

	pub(crate) async fn generate_snapshots(&self, granularity_interval: u64, snapshot_interval: u64, snapshot_scopes: &[u64], cache_path: &str, max_symlink_count: Option<u64>) {
		let pending_snapshot_directory = format!("{}/snapshots_pending", cache_path);
		let pending_symlink_directory = format!("{}/symlinks_pending", cache_path);
		let finalized_snapshot_directory = format!("{}/snapshots", cache_path);
		let finalized_symlink_directory = format!("{}/symlinks", cache_path);
		let relative_symlink_to_snapshot_path = "../snapshots";

		// 1. get the current timestamp
		let snapshot_generation_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
		let reference_timestamp = Self::round_down_to_nearest_multiple(snapshot_generation_timestamp, snapshot_interval as u64);
		log_info!(self.logger, "Capturing snapshots at {} for: {}", snapshot_generation_timestamp, reference_timestamp);

		// 2. sleep until the next round interval
		// 3. refresh all snapshots

		// the stored snapshots should adhere to the following format
		// from one day ago
		// from two days ago
		// …
		// from a week ago
		// from two weeks ago
		// from three weeks ago
		// full
		// That means that at any given moment, there should only ever be
		// 6 (daily) + 3 (weekly) + 1 (total) = 10 cached snapshots
		// The snapshots, unlike dynamic updates, should account for all intermediate
		// channel updates

		// purge and recreate the pending directories
		if fs::metadata(&pending_snapshot_directory).is_ok() {
			fs::remove_dir_all(&pending_snapshot_directory).expect("Failed to remove pending snapshot directory.");
		}
		if fs::metadata(&pending_symlink_directory).is_ok() {
			fs::remove_dir_all(&pending_symlink_directory).expect("Failed to remove pending symlink directory.");
		}
		fs::create_dir_all(&pending_snapshot_directory).expect("Failed to create pending snapshot directory");
		fs::create_dir_all(&pending_symlink_directory).expect("Failed to create pending symlink directory");

		let mut snapshot_sync_timestamps: Vec<(u64, u64)> = Vec::new();
		for current_scope in snapshot_scopes {
			let timestamp = reference_timestamp.saturating_sub(current_scope.clone());
			snapshot_sync_timestamps.push((current_scope.clone(), timestamp));
		};

		let mut snapshot_filenames_by_scope: HashMap<u64, String> = HashMap::with_capacity(10);

		for (current_scope, current_last_sync_timestamp) in &snapshot_sync_timestamps {
			let network_graph_clone = self.network_graph.clone();
			{
				log_info!(self.logger, "Calculating {}-second snapshot", current_scope);
				// calculate the snapshot
				let snapshot = super::serialize_delta(network_graph_clone, current_last_sync_timestamp.clone() as u32, self.logger.clone()).await;

				// persist the snapshot and update the symlink
				let snapshot_filename = format!("snapshot__calculated-at:{}__range:{}-scope__previous-sync:{}.lngossip", reference_timestamp, current_scope, current_last_sync_timestamp);
				let snapshot_path = format!("{}/{}", pending_snapshot_directory, snapshot_filename);
				log_info!(self.logger, "Persisting {}-second snapshot: {} ({} messages, {} announcements, {} updates ({} full, {} incremental))", current_scope, snapshot_filename, snapshot.message_count, snapshot.announcement_count, snapshot.update_count, snapshot.update_count_full, snapshot.update_count_incremental);
				fs::write(&snapshot_path, snapshot.data.clone()).unwrap();
				snapshot_filenames_by_scope.insert(current_scope.clone(), snapshot_filename);

                    // after snapshot, upload results to a server
                    // only doing this for 0 for now
                    if let Some(api_key) = config::upload_api_key() {
                        if *current_scope == u64::MAX {
                            let client = crate::client::Client::new();
                            match client.post_snapshot(snapshot, 0, api_key) {
                                Ok(_) => {
					                log_info!(self.logger, "posted snapshot: {}", 0);
                                },
                                Err(e) => {
					                log_error!(self.logger, "error posted snapshot: {}", e);
                                },
                            }
                        }
                    }
			}
		}

		{
			// create dummy symlink
			let dummy_filename = "empty_delta.lngossip";
			let dummy_snapshot = super::serialize_empty_blob(reference_timestamp);
			let dummy_snapshot_path = format!("{}/{}", pending_snapshot_directory, dummy_filename);
			fs::write(&dummy_snapshot_path, dummy_snapshot).unwrap();

			let dummy_symlink_path = format!("{}/{}.bin", pending_symlink_directory, reference_timestamp);
			let relative_dummy_snapshot_path = format!("{}/{}", relative_symlink_to_snapshot_path, dummy_filename);
			log_info!(self.logger, "Symlinking dummy: {} -> {}", dummy_symlink_path, relative_dummy_snapshot_path);
			symlink(&relative_dummy_snapshot_path, &dummy_symlink_path).unwrap();
		}

		// Number of intervals since Jan 1, 2022, a few months before RGS server was released.
		let mut symlink_count = (reference_timestamp - 1640995200) / granularity_interval;
		if let Some(max_symlink_count) = max_symlink_count {
			// this is primarily useful for testing
			symlink_count = std::cmp::min(symlink_count, max_symlink_count);
		};

		for i in 0..symlink_count {
			// let's create non-dummy-symlinks

			// first, determine which snapshot range should be referenced
			let referenced_scope = if i == 0 {
				// special-case 0 to always refer to a full/initial sync
				u64::MAX
			} else {
				/*
				We have snapshots for 6-day- and 7-day-intervals, but the next interval is
				14 days. So if somebody requests an update with a timestamp that is 10 days old,
				there is no longer a snapshot for that specific interval.

				The correct snapshot will be the next highest interval, i. e. for 14 days.

				The `snapshot_sync_day_factors` array is sorted ascendingly, so find() will
				return on the first iteration that is at least equal to the requested interval.

				Note, however, that the last value in the array is u64::max, which means that
				multiplying it with snapshot_interval will overflow. To avoid that, we use
				saturating_mul.
				 */

				// find min(x) in snapshot_scopes where i * granularity <= x (the current scope)
				snapshot_scopes.iter().find(|current_scope| {
					i * granularity_interval <= **current_scope
				}).unwrap().clone()
			};
			log_info!(self.logger, "i: {}, referenced scope: {}", i, referenced_scope);

			let snapshot_filename = snapshot_filenames_by_scope.get(&referenced_scope).unwrap();
			let relative_snapshot_path = format!("{}/{}", relative_symlink_to_snapshot_path, snapshot_filename);

			let canonical_last_sync_timestamp = if i == 0 {
				// special-case 0 to always refer to a full/initial sync
				0
			} else {
				reference_timestamp.saturating_sub(granularity_interval.saturating_mul(i))
			};
			let symlink_path = format!("{}/{}.bin", pending_symlink_directory, canonical_last_sync_timestamp);

			log_info!(self.logger, "Symlinking: {} -> {} ({} -> {}", i, referenced_scope, symlink_path, relative_snapshot_path);
			symlink(&relative_snapshot_path, &symlink_path).unwrap();
		}

		let update_time_path = format!("{}/update_time.txt", pending_symlink_directory);
		let update_time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
		fs::write(&update_time_path, format!("{}", update_time)).unwrap();

		if fs::metadata(&finalized_snapshot_directory).is_ok() {
			fs::remove_dir_all(&finalized_snapshot_directory).expect("Failed to remove finalized snapshot directory.");
		}
		if fs::metadata(&finalized_symlink_directory).is_ok() {
			fs::remove_dir_all(&finalized_symlink_directory).expect("Failed to remove pending symlink directory.");
		}
		fs::rename(&pending_snapshot_directory, &finalized_snapshot_directory).expect("Failed to finalize snapshot directory.");
		fs::rename(&pending_symlink_directory, &finalized_symlink_directory).expect("Failed to finalize symlink directory.");
	}

	pub(super) fn round_down_to_nearest_multiple(number: u64, multiple: u64) -> u64 {
		let round_multiple_delta = number % multiple;
		number - round_multiple_delta
	}
}
