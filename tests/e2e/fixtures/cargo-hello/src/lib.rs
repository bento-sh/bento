pub fn greeting(name: &str) -> String {
    format!("hello, {name}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn greeting_is_friendly() {
        assert_eq!(super::greeting("world"), "hello, world");
    }
}
