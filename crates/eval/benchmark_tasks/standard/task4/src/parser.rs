pub fn parse_csv(data: &str) -> Vec<Vec<String>> {
    data.lines()
        .map(|line| line.split(',').map(String::from).collect())
        .collect()
}
