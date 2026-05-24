pub mod server;
pub mod routes;
pub mod logging;

pub use server::AbacusServer;

#[cfg(test)]
mod tests {
    use super::routes::*;

    #[test]
    fn test_health_response() {
        let r = HealthResponse {
            status: "ok".into(),
            version: "0.1.0".into(),
            session_count: 0,
            team_count: 0,
            model_count: 0,
        };
        assert_eq!(r.status, "ok");
    }

    #[test]
    fn test_chat_request_deserialize() {
        let json = r#"{"message":"hello","session_id":null}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hello");
        assert!(req.session_id.is_none());
    }

    #[test]
    fn test_chat_response_serialize() {
        let resp = ChatResponse {
            response: "hi".into(),
            session_id: "sess_123".into(),
            tool_calls: 2,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("hi"));
        assert!(json.contains("sess_123"));
    }

    #[test]
    fn test_session_info_serialize() {
        let info = SessionInfo {
            session_id: "sess_abc".into(),
            turn_count: 5,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("sess_abc"));
        assert!(json.contains("5"));
    }

    #[test]
    fn test_rate_limiter_basic() {
        use super::server::RateLimiter;
        let rl = RateLimiter::new(2, 1); // 2 requests per second
        let key = "test_client";
        assert!(rl.check(key)); // 1st request
        assert!(rl.check(key)); // 2nd request
        assert!(!rl.check(key)); // 3rd request — rate limited
    }
}

