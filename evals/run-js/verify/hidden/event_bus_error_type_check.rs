// verify/t4-build-test-v3.sh が検証時だけ crates/event-bus/tests/ にコピーする隠しテスト。
use event_bus::event::ProviderFailureKind as K;

#[test]
fn error_type_values_are_fixed() {
    assert_eq!(K::RateLimited.error_type(), "rate_limited");
    assert_eq!(K::Http { status: 418 }.error_type(), "http");
    assert_eq!(K::Timeout.error_type(), "timeout");
    assert_eq!(K::InvalidResponse.error_type(), "invalid_response");
    assert_eq!(K::Transport.error_type(), "transport");
    assert_eq!(K::Server.error_type(), "server");
    assert_eq!(K::Quota.error_type(), "quota");
    assert_eq!(K::Auth.error_type(), "auth");
    assert_eq!(K::Other.error_type(), "other");
    const T: &str = K::Timeout.error_type();
    assert_eq!(T, "timeout");
}
