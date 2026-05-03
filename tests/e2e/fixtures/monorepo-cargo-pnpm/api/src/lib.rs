pub fn endpoint(path: &str) -> String {
    format!("GET {path} -> 200 OK")
}

#[cfg(test)]
mod tests {
    #[test]
    fn health_endpoint_responds() {
        assert_eq!(super::endpoint("/health"), "GET /health -> 200 OK");
    }
}
