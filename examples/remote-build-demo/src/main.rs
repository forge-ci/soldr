fn message() -> &'static str {
    "hello from the Soldr runner"
}

fn main() {
    println!("{}", message());
}

#[cfg(test)]
mod tests {
    #[test]
    fn demo_message() {
        assert_eq!(super::message(), "hello from the Soldr runner");
    }
}
