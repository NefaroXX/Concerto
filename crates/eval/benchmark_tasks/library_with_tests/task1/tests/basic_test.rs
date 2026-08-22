use string_util_lib::{to_camel_case, to_kebab_case, to_snake_case};

#[test]
fn test_snake_case() {
    assert_eq!(to_snake_case("HelloWorld"), "hello_world");
    assert_eq!(to_snake_case("Simple"), "simple");
}

#[test]
fn test_camel_case() {
    assert_eq!(to_camel_case("hello_world"), "helloWorld");
    assert_eq!(to_camel_case("kebab-case"), "kebabCase");
}

#[test]
fn test_kebab_case() {
    assert_eq!(to_kebab_case("HelloWorld"), "hello-world");
    assert_eq!(to_kebab_case("snake_case"), "snake-case");
}

#[test]
fn test_round_trip() {
    let original = "hello_world";
    let camel = to_camel_case(original);
    let snake = to_snake_case(&camel);
    assert_eq!(snake, original);
}
