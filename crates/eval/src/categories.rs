/// Categorized benchmark task directories.
#[non_exhaustive]
pub enum BenchmarkCategory {
    SimpleCliTool,
    SmallWebApi,
    LibraryWithTests,
    BugFix,
}

impl BenchmarkCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SimpleCliTool => "simple_cli_tool",
            Self::SmallWebApi => "small_web_api",
            Self::LibraryWithTests => "library_with_tests",
            Self::BugFix => "bug_fix",
        }
    }

    pub fn all() -> Vec<Self> {
        vec![Self::SimpleCliTool, Self::SmallWebApi, Self::LibraryWithTests, Self::BugFix]
    }

    /// Dirname relative to benchmark_tasks root.
    pub fn dirname(&self) -> &'static str {
        self.as_str()
    }

    /// Number of tasks in this category (for reporting)
    pub fn task_count(&self) -> usize {
        match self {
            Self::SimpleCliTool => 2,
            Self::SmallWebApi => 2,
            Self::LibraryWithTests => 2,
            Self::BugFix => 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_returns_expected_strings() {
        assert_eq!(BenchmarkCategory::SimpleCliTool.as_str(), "simple_cli_tool");
        assert_eq!(BenchmarkCategory::SmallWebApi.as_str(), "small_web_api");
        assert_eq!(BenchmarkCategory::LibraryWithTests.as_str(), "library_with_tests");
        assert_eq!(BenchmarkCategory::BugFix.as_str(), "bug_fix");
    }

    #[test]
    fn all_returns_four_categories() {
        let all = BenchmarkCategory::all();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn dirname_matches_as_str() {
        for cat in BenchmarkCategory::all() {
            assert_eq!(cat.dirname(), cat.as_str());
        }
    }

    #[test]
    fn task_counts_are_positive() {
        for cat in BenchmarkCategory::all() {
            assert!(
                cat.task_count() > 0,
                "category `{}` should have a positive count",
                cat.as_str()
            );
        }
    }
}
