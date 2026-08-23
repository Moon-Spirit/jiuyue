//! JiuYue wire protocol v1.
//!
//! Versioned WebSocket frame envelope `{ "v": 1, "t": "<type>", "d": {...} }`
//! shared by client and server. Frame types are added per milestone; unknown
//! `t` values must deserialize into a typed error frame (forward compat).

/// Envelope version marker for all v1 frames.
pub const PROTOCOL_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::PROTOCOL_VERSION;

    #[test]
    fn protocol_version_is_one() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
