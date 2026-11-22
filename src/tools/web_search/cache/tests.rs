use super::*;

fn hit(n: usize) -> SearchHit {
    SearchHit {
        title: format!("title {n}"),
        url: format!("https://example.com/{n}"),
        snippet: String::new(),
    }
}

fn hits(n: usize) -> Vec<SearchHit> {
    (0..n).map(hit).collect()
}

const TTL: Duration = Duration::from_secs(60);

#[tokio::test(start_paused = true)]
async fn a_stored_search_is_returned() {
    let cache = ResultCache::new(TTL, 8);
    assert_eq!(cache.get("wt-wt", "rust", 5), None);
    cache.put("wt-wt", "rust", 5, &hits(3));
    assert_eq!(cache.get("wt-wt", "rust", 5), Some(hits(3)));
}

#[tokio::test(start_paused = true)]
async fn the_key_ignores_case_and_spacing_but_not_region() {
    let cache = ResultCache::new(TTL, 8);
    cache.put("wt-wt", "Rust  Async\tbook", 5, &hits(2));
    assert!(cache.get("wt-wt", "  rust async book ", 5).is_some());
    assert!(cache.get("us-en", "rust async book", 5).is_none());
}

#[tokio::test(start_paused = true)]
async fn an_entry_expires_after_the_ttl() {
    let cache = ResultCache::new(TTL, 8);
    cache.put("wt-wt", "q", 5, &hits(2));
    tokio::time::advance(TTL - Duration::from_secs(1)).await;
    assert!(cache.get("wt-wt", "q", 5).is_some(), "still fresh");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(cache.get("wt-wt", "q", 5).is_none(), "expired at the ttl");
    assert_eq!(cache.len(), 0, "the expired entry is removed on lookup");
}

#[tokio::test(start_paused = true)]
async fn a_put_sweeps_expired_entries() {
    let cache = ResultCache::new(TTL, 8);
    cache.put("wt-wt", "old", 5, &hits(1));
    tokio::time::advance(TTL).await;
    cache.put("wt-wt", "new", 5, &hits(1));
    assert_eq!(cache.len(), 1, "only the fresh entry is held");
}

#[tokio::test(start_paused = true)]
async fn a_put_refreshes_the_entry() {
    let cache = ResultCache::new(TTL, 8);
    cache.put("wt-wt", "q", 5, &hits(1));
    tokio::time::advance(Duration::from_secs(50)).await;
    cache.put("wt-wt", "q", 5, &hits(2));
    tokio::time::advance(Duration::from_secs(50)).await;
    assert_eq!(cache.get("wt-wt", "q", 5), Some(hits(2)));
}

#[tokio::test(start_paused = true)]
async fn empty_results_are_not_stored() {
    let cache = ResultCache::new(TTL, 8);
    cache.put("wt-wt", "q", 5, &[]);
    assert_eq!(cache.len(), 0);
    assert_eq!(cache.get("wt-wt", "q", 5), None);
}

#[tokio::test(start_paused = true)]
async fn the_oldest_entry_is_evicted_at_capacity() {
    let cache = ResultCache::new(TTL, 2);
    cache.put("wt-wt", "a", 5, &hits(1));
    tokio::time::advance(Duration::from_secs(1)).await;
    cache.put("wt-wt", "b", 5, &hits(1));
    tokio::time::advance(Duration::from_secs(1)).await;
    cache.put("wt-wt", "c", 5, &hits(1));
    assert_eq!(cache.len(), 2);
    assert!(cache.get("wt-wt", "a", 5).is_none(), "oldest evicted");
    assert!(cache.get("wt-wt", "b", 5).is_some());
    assert!(cache.get("wt-wt", "c", 5).is_some());
}

#[tokio::test(start_paused = true)]
async fn replacing_an_entry_at_capacity_evicts_nothing() {
    let cache = ResultCache::new(TTL, 2);
    cache.put("wt-wt", "a", 5, &hits(1));
    cache.put("wt-wt", "b", 5, &hits(1));
    cache.put("wt-wt", "a", 5, &hits(2));
    assert!(cache.get("wt-wt", "b", 5).is_some());
    assert_eq!(cache.get("wt-wt", "a", 5), Some(hits(2)));
}

#[tokio::test(start_paused = true)]
async fn a_smaller_request_is_served_from_a_larger_one() {
    let cache = ResultCache::new(TTL, 8);
    cache.put("wt-wt", "q", 5, &hits(5));
    assert_eq!(cache.get("wt-wt", "q", 2), Some(hits(2)));
}

#[tokio::test(start_paused = true)]
async fn a_larger_request_misses_unless_the_page_ran_out() {
    let cache = ResultCache::new(TTL, 8);
    // Asked for 3 and got 3: there may be more, so asking for 5 is a miss.
    cache.put("wt-wt", "full", 3, &hits(3));
    assert_eq!(cache.get("wt-wt", "full", 5), None);
    // Asked for 5 and got 2: the page had only 2, so 5 is answered by them.
    cache.put("wt-wt", "short", 5, &hits(2));
    assert_eq!(cache.get("wt-wt", "short", 5), Some(hits(2)));
    assert_eq!(cache.get("wt-wt", "short", 10), Some(hits(2)));
}
