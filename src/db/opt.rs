use super::*;

#[derive(Clone)]
pub struct AgateOptions {
    pub dir: PathBuf,
    pub value_dir: PathBuf,
    // TODO: docs
    pub in_memory: bool,
    pub sync_writes: bool,

    // Memtable options
    pub mem_table_size: u64,
    pub base_table_size: u64,
    pub base_level_size: u64,
    pub level_size_multiplier: usize,
    pub table_size_multiplier: usize,
    pub max_levels: usize,

    pub value_threshold: usize,
    pub num_memtables: usize,

    pub block_size: usize,
    pub bloom_false_positive: f64,

    pub num_level_zero_tables: usize,
    pub num_level_zero_tables_stall: usize,

    pub value_log_file_size: u64,
    pub value_log_max_entries: u32,
}

impl Default for AgateOptions {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
            value_dir: PathBuf::new(),
            // memtable options
            mem_table_size: 64 << 20,
            base_table_size: 2 << 20,
            base_level_size: 10 << 20,
            table_size_multiplier: 2,
            level_size_multiplier: 10,
            max_levels: 7,
            // agate options
            num_memtables: 20,
            in_memory: false,
            sync_writes: false,
            value_threshold: 1 << 10,
            value_log_file_size: 1 << 30 - 1,
            value_log_max_entries: 1000000,
            block_size: 4 << 10,
            bloom_false_positive: 0.01,
            num_level_zero_tables: 5,
            num_level_zero_tables_stall: 15,
        }
        // TODO: add other options
    }
}

impl AgateOptions {
    pub(crate) fn fix_options(&mut self) -> Result<()> {
        if self.in_memory {
            // TODO: find a way to check if path is set, if set, then panic with ConfigError
            self.sync_writes = false;
        }

        Ok(())
    }

    pub fn skip_vlog(&self, entry: &Entry) -> bool {
        entry.value.len() < self.value_threshold
    }

    fn arena_size(&self) -> u64 {
        // TODO: take other options into account
        self.mem_table_size as u64
    }
}
