use base64::{Engine as _, engine::general_purpose::STANDARD};
use subtle::ConstantTimeEq;

pub struct Authenticator {
    expected: Option<Vec<u8>>,
}

impl Authenticator {
    pub fn required(username: &str, password: &str) -> Self {
        Self {
            expected: Some(format!("{username}:{password}").into_bytes()),
        }
    }

    pub fn disabled() -> Self {
        Self { expected: None }
    }

    pub fn is_authorized(&self, header: Option<&str>) -> bool {
        let Some(expected) = &self.expected else {
            return true;
        };
        let Some(header) = header else {
            return false;
        };
        let Some((scheme, encoded)) = header.split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("Basic") {
            return false;
        }

        let Ok(provided) = STANDARD.decode(encoded.trim()) else {
            return false;
        };
        if provided.len() != expected.len() {
            return false;
        }

        bool::from(provided.ct_eq(expected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_basic_credentials() {
        let auth = Authenticator::required("proxy-user", "correct horse");
        assert!(auth.is_authorized(Some("Basic cHJveHktdXNlcjpjb3JyZWN0IGhvcnNl")));
        assert!(!auth.is_authorized(Some("Basic cHJveHktdXNlcjp3cm9uZw==")));
        assert!(!auth.is_authorized(None));
    }
}
