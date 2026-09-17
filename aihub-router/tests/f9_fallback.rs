use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aihub_core::TaskTier;
use aihub_router::{classify, classify_with_fallback_using, Classification};

#[tokio::test]
async fn f9_clear_prompt_skips_injected_fallback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    let fallback = move |_prompt: &str| {
        let calls_cb = Arc::clone(&calls_cb);
        async move {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Some(Classification {
                tier: TaskTier::Review,
                confidence: 1.0,
                ambiguous: false,
            })
        }
    };
    let answer = classify_with_fallback_using("refactor this function", fallback).await;
    assert_eq!(answer.tier, TaskTier::Mechanical);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn f9_ambiguous_prompt_invokes_injected_fallback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    let fallback = move |_prompt: &str| {
        let calls_cb = Arc::clone(&calls_cb);
        async move {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Some(Classification {
                tier: TaskTier::Review,
                confidence: 0.95,
                ambiguous: false,
            })
        }
    };
    let answer = classify_with_fallback_using("refatora e revisa", fallback).await;
    assert_eq!(answer.tier, TaskTier::Review);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(classify("refatora e revisa").ambiguous);
}

#[tokio::test]
async fn f9_fallback_failure_returns_heuristic() {
    let fallback = |_prompt: &str| async { None::<Classification> };
    let answer = classify_with_fallback_using("hello", fallback).await;
    assert_eq!(answer, classify("hello"));
}
