use cortex::constants;

crate::dedup_wrapper!(
    /// Bounded deduplicator for Telegram update IDs.
    pub struct UpdateDeduplicator(i64, constants::TELEGRAM_DEDUP_CAPACITY);
    is_duplicate(update_id: i64) => update_id;
);
