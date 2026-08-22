use benchmark_task_5::*;

#[test]
fn test_reverse() {
    assert_eq!(reverse("hello"), "olleh");
    assert_eq!(reverse("a"), "a");
    assert_eq!(reverse(""), "");
    assert_eq!(reverse("café"), "éfac");
    assert_eq!(reverse("🎉 party"), "ytrap 🎉");
}
