/// Pool of agent nicknames with deduplication.
pub struct NamePool {
    pool: Vec<&'static str>,
    index: usize,
}

const DEFAULT_NAMES: &[&str] = &[
    "Atlas",
    "Aurora",
    "Blueprint",
    "Catalyst",
    "Delta",
    "Echo",
    "Forge",
    "Horizon",
    "Ion",
    "Jade",
    "Keystone",
    "Lumen",
    "Meridian",
    "Nexus",
    "Orbit",
    "Prism",
    "Quartz",
    "Relay",
    "Spark",
    "Tensor",
];

impl Default for NamePool {
    fn default() -> Self {
        Self::new()
    }
}

impl NamePool {
    pub fn new() -> Self {
        Self {
            pool: DEFAULT_NAMES.to_vec(),
            index: 0,
        }
    }

    /// Get the next unique name. Adds ordinal suffix on exhaustion.
    pub fn next_name(&mut self) -> String {
        let base = self.pool[self.index % self.pool.len()];
        self.index += 1;

        let gen = self.index.saturating_sub(1) / self.pool.len();
        if gen == 0 {
            base.to_string()
        } else {
            format!("{base} the {}", ordinal(gen + 1))
        }
    }
}

fn ordinal(n: usize) -> String {
    match n {
        2 => "2nd".to_string(),
        3 => "3rd".to_string(),
        _ => format!("{n}th"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nickname_generation_unique() {
        let mut pool = NamePool::new();
        let mut names = Vec::new();
        for _ in 0..DEFAULT_NAMES.len() {
            names.push(pool.next_name());
        }
        // All names should be unique
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn test_nickname_generation_ordinal_suffix() {
        let mut pool = NamePool::new();
        // Exhaust first round
        for _ in 0..DEFAULT_NAMES.len() {
            pool.next_name();
        }
        // Next name should have ordinal suffix
        let name = pool.next_name();
        assert!(
            name.contains("the 2nd"),
            "Expected ordinal suffix, got: {name}"
        );
    }

    #[test]
    fn test_ordinal_formatting() {
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(3), "3rd");
        assert_eq!(ordinal(4), "4th");
        assert_eq!(ordinal(5), "5th");
    }

    #[test]
    fn test_name_pool_first_name() {
        let mut pool = NamePool::new();
        assert_eq!(pool.next_name(), "Atlas");
    }

    #[test]
    fn test_third_cycle_ordinal() {
        let mut pool = NamePool::new();
        for _ in 0..(DEFAULT_NAMES.len() * 2) {
            pool.next_name();
        }
        let name = pool.next_name();
        assert!(
            name.contains("the 3rd"),
            "Expected 'the 3rd' suffix, got: {name}"
        );
    }

    #[test]
    fn test_all_first_round_names_are_base_names() {
        let mut pool = NamePool::new();
        for _ in 0..DEFAULT_NAMES.len() {
            let name = pool.next_name();
            assert!(
                !name.contains("the"),
                "First round name should not have ordinal suffix, got: {name}"
            );
        }
    }
}
