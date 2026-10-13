use probe_locate_scale::calc::scaled_sum;
use probe_locate_scale::config::SCALE;

#[test]
fn scaled_sum_uses_the_project_scale() {
    assert_eq!(SCALE, 7);
    assert_eq!(scaled_sum(1, 2), 21);
    assert_eq!(scaled_sum(0, 0), 0);
}
