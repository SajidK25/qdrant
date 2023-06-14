use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use segment::common::operation_time_statistics::{
    OperationDurationStatistics, OperationDurationsAggregator,
};
use segment::types::{HnswConfig, Indexes, QuantizationConfig, SegmentType, VECTOR_ELEMENT_SIZE};

use crate::collection_manager::holders::segment_holder::{LockedSegmentHolder, SegmentId};
use crate::collection_manager::optimizers::segment_optimizer::{
    OptimizerThresholds, SegmentOptimizer,
};
use crate::config::CollectionParams;
use crate::operations::config_diff::DiffConfig;

/// Looks for segments having a mismatch between configured and actual parameters
///
/// For example, a user may change the HNSW parameters for a collection. A segment that was already
/// indexed with different parameters now has a mismatch. This segment should be optimized (and
/// indexed) again in order to update the effective configuration.
pub struct ConfigMismatchOptimizer {
    thresholds_config: OptimizerThresholds,
    segments_path: PathBuf,
    collection_temp_dir: PathBuf,
    collection_params: CollectionParams,
    hnsw_config: HnswConfig,
    quantization_config: Option<QuantizationConfig>,
    telemetry_durations_aggregator: Arc<Mutex<OperationDurationsAggregator>>,
}

impl ConfigMismatchOptimizer {
    pub fn new(
        thresholds_config: OptimizerThresholds,
        segments_path: PathBuf,
        collection_temp_dir: PathBuf,
        collection_params: CollectionParams,
        hnsw_config: HnswConfig,
        quantization_config: Option<QuantizationConfig>,
    ) -> Self {
        ConfigMismatchOptimizer {
            thresholds_config,
            segments_path,
            collection_temp_dir,
            collection_params,
            hnsw_config,
            quantization_config,
            telemetry_durations_aggregator: OperationDurationsAggregator::new(),
        }
    }

    fn worst_segment(
        &self,
        segments: LockedSegmentHolder,
        excluded_ids: &HashSet<SegmentId>,
    ) -> Vec<SegmentId> {
        let segments_read_guard = segments.read();
        let candidates: Vec<_> = segments_read_guard
            .iter()
            // Excluded externally, might already be scheduled for optimization
            .filter(|(idx, _)| !excluded_ids.contains(idx))
            .filter_map(|(idx, segment)| {
                let segment_entry = segment.get();
                let read_segment = segment_entry.read();
                let point_count = read_segment.available_point_count();
                let vector_size = point_count
                    * read_segment
                        .vector_dims()
                        .values()
                        .max()
                        .copied()
                        .unwrap_or(0)
                    * VECTOR_ELEMENT_SIZE;

                let segment_config = read_segment.config();

                if read_segment.segment_type() == SegmentType::Special {
                    return None; // Never optimize already optimized segment
                }

                // Determine whether segment has mismatch
                let has_mismatch =
                    segment_config
                        .vector_data
                        .iter()
                        .any(|(vector_name, vector_data)| {
                            // Check HNSW mismatch
                            match &vector_data.index {
                                Indexes::Plain {} => {}
                                Indexes::Hnsw(effective_hnsw) => {
                                    // Select vector specific target HNSW config
                                    let target_hnsw_collection = &self.hnsw_config;
                                    let target_hnsw_vector = self
                                        .collection_params
                                        .vectors
                                        .get_params(vector_name)
                                        .and_then(|vector_params| vector_params.hnsw_config)
                                        .map(|vector_hnsw| vector_hnsw.update(target_hnsw_collection))
                                        .and_then(|hnsw| match hnsw {
                                            Ok(hnsw) => Some(hnsw),
                                            Err(err) => {
                                                log::warn!("Failed to merge collection and vector HNSW config, ignoring: {err}");
                                                None
                                            }
                                        });
                                    let target_hnsw = target_hnsw_vector
                                        .as_ref()
                                        .unwrap_or(target_hnsw_collection);

                                    // Select segment if we have an HNSW mismatch that requires rebuild
                                    if effective_hnsw.mismatch_requires_rebuild(target_hnsw) {
                                        return true;
                                    }
                                }
                            }

                            false
                        });

                has_mismatch.then_some((*idx, vector_size))
            })
            .collect();

        // Select segment with largest vector size
        candidates
            .into_iter()
            .max_by_key(|(_, vector_size)| *vector_size)
            .map(|(segment_id, _)| segment_id)
            .into_iter()
            .collect()
    }
}

impl SegmentOptimizer for ConfigMismatchOptimizer {
    fn collection_path(&self) -> &Path {
        self.segments_path.as_path()
    }

    fn temp_path(&self) -> &Path {
        self.collection_temp_dir.as_path()
    }

    fn collection_params(&self) -> CollectionParams {
        self.collection_params.clone()
    }

    fn hnsw_config(&self) -> &HnswConfig {
        &self.hnsw_config
    }

    fn quantization_config(&self) -> Option<QuantizationConfig> {
        self.quantization_config.clone()
    }

    fn threshold_config(&self) -> &OptimizerThresholds {
        &self.thresholds_config
    }

    fn check_condition(
        &self,
        segments: LockedSegmentHolder,
        excluded_ids: &HashSet<SegmentId>,
    ) -> Vec<SegmentId> {
        self.worst_segment(segments, excluded_ids)
    }

    fn get_telemetry_data(&self) -> OperationDurationStatistics {
        self.get_telemetry_counter().lock().get_statistics()
    }

    fn get_telemetry_counter(&self) -> Arc<Mutex<OperationDurationsAggregator>> {
        self.telemetry_durations_aggregator.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use parking_lot::RwLock;
    use segment::entry::entry_point::SegmentEntry;
    use segment::types::Distance;
    use tempfile::Builder;

    use super::*;
    use crate::collection_manager::fixtures::random_multi_vec_segment;
    use crate::collection_manager::holders::segment_holder::{LockedSegment, SegmentHolder};
    use crate::collection_manager::optimizers::indexing_optimizer::IndexingOptimizer;
    use crate::operations::types::{VectorParams, VectorsConfig};

    /// This test the config mismatch optimizer for a changed HNSW config.
    ///
    /// It tests whether:
    /// - the condition check for HNSW mismatches works
    /// - optimized segments (and vector storages) use the updated configuration
    ///
    /// In short, this is what happens in this test:
    /// - create randomized multi segment as base
    /// - use indexing optimizer to build index for our segment
    /// - test config mismatch condition: should not trigger yet
    /// - change collection HNSW config
    /// - test config mismatch condition: should trigger due to HNSW change
    /// - optimize segment with config mismatch optimizer
    /// - assert segment uses changed configuration
    #[test]
    fn test_hnsw_config_mismatch() {
        // Collection configuration
        let (point_count, vector1_dim, vector2_dim) = (1000, 10, 20);
        let thresholds_config = OptimizerThresholds {
            max_segment_size: std::usize::MAX,
            memmap_threshold: std::usize::MAX,
            indexing_threshold: 10,
        };
        let collection_params = CollectionParams {
            vectors: VectorsConfig::Multi(BTreeMap::from([
                (
                    "vector1".into(),
                    VectorParams {
                        size: vector1_dim.try_into().unwrap(),
                        distance: Distance::Dot,
                        hnsw_config: None,
                        quantization_config: None,
                        on_disk: None,
                    },
                ),
                (
                    "vector2".into(),
                    VectorParams {
                        size: vector2_dim.try_into().unwrap(),
                        distance: Distance::Dot,
                        hnsw_config: None,
                        quantization_config: None,
                        on_disk: None,
                    },
                ),
            ])),
            shard_number: 1.try_into().unwrap(),
            on_disk_payload: false,
            replication_factor: 1.try_into().unwrap(),
            write_consistency_factor: 1.try_into().unwrap(),
        };

        // Base segment
        let temp_dir = Builder::new().prefix("segment_temp_dir").tempdir().unwrap();
        let dir = Builder::new().prefix("segment_dir").tempdir().unwrap();
        let mut holder = SegmentHolder::default();

        let segment = random_multi_vec_segment(
            dir.path(),
            100,
            point_count,
            vector1_dim as usize,
            vector2_dim as usize,
        );

        let segment_id = holder.add(segment);
        let locked_holder: Arc<RwLock<_>> = Arc::new(RwLock::new(holder));

        let hnsw_config = HnswConfig {
            m: 16,
            ef_construct: 100,
            full_scan_threshold: 10, // Force to build HNSW links for payload
            max_indexing_threads: 0,
            on_disk: None,
            payload_m: None,
        };

        // Optimizers used in test
        let index_optimizer = IndexingOptimizer::new(
            thresholds_config.clone(),
            dir.path().to_owned(),
            temp_dir.path().to_owned(),
            collection_params.clone(),
            hnsw_config.clone(),
            Default::default(),
        );
        let mut config_mismatch_optimizer = ConfigMismatchOptimizer::new(
            thresholds_config,
            dir.path().to_owned(),
            temp_dir.path().to_owned(),
            collection_params,
            hnsw_config.clone(),
            Default::default(),
        );

        // Use indexing optimizer to build index for HNSW mismatch test
        let changed = index_optimizer
            .optimize(locked_holder.clone(), vec![segment_id], &false.into())
            .unwrap();
        assert!(changed, "optimizer should have rebuilt this segment");
        assert!(
            locked_holder.read().get(segment_id).is_none(),
            "optimized segment should be gone",
        );
        assert_eq!(locked_holder.read().len(), 2, "index must be built");

        // Mismatch optimizer should not optimize yet, HNSW config is not changed yet
        let suggested_to_optimize =
            config_mismatch_optimizer.check_condition(locked_holder.clone(), &Default::default());
        assert_eq!(suggested_to_optimize.len(), 0);

        // Create changed HNSW config with other m/ef_construct value, update it in the optimizer
        let mut changed_hnsw_config = hnsw_config;
        changed_hnsw_config.m /= 2;
        changed_hnsw_config.ef_construct /= 5;
        config_mismatch_optimizer.hnsw_config = changed_hnsw_config.clone();

        // Run mismatch optimizer again, make sure it optimizes now
        let suggested_to_optimize =
            config_mismatch_optimizer.check_condition(locked_holder.clone(), &Default::default());
        assert_eq!(suggested_to_optimize.len(), 1);
        let changed = config_mismatch_optimizer
            .optimize(locked_holder.clone(), suggested_to_optimize, &false.into())
            .unwrap();
        assert!(changed, "optimizer should have rebuilt this segment");

        // Ensure new segment has changed HNSW config
        locked_holder
            .read()
            .iter()
            .map(|(_, segment)| match segment {
                LockedSegment::Original(s) => s.read(),
                LockedSegment::Proxy(_) => unreachable!(),
            })
            .filter(|segment| segment.total_point_count() > 0)
            .for_each(|segment| {
                segment
                    .config()
                    .vector_data
                    .values()
                    .map(|vector_data| &vector_data.index)
                    .filter_map(|index| match index {
                        Indexes::Plain {} => None,
                        Indexes::Hnsw(hnsw) => Some(hnsw),
                    })
                    .for_each(|hnsw| {
                        assert_eq!(
                            hnsw, &changed_hnsw_config,
                            "segment must be optimized with changed HNSW config",
                        );
                    });
            });
    }
}
