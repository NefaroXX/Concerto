use benchmark_task_4::parser;
use benchmark_task_4::legacy;

#[test]
fn test_parser_parse_csv() {
    let result = parser::parse_csv("a,b,c\n1,2,3");
    assert_eq!(result, vec![
        vec!["a", "b", "c"],
        vec!["1", "2", "3"],
    ]);
}

#[test]
fn test_legacy_parse_csv() {
    let result = legacy::parse_csv("x,y\n10,20");
    assert_eq!(result, vec![
        vec!["x", "y"],
        vec!["10", "20"],
    ]);
}
