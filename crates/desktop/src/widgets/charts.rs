use iced::widget::{column, container, text};
use iced::Element;

#[derive(Debug, Clone)]
pub enum ChartMessage {}

#[derive(Debug, Clone)]
pub enum ChartKind {
    Bar { values: Vec<f32>, labels: Vec<String> },
    Donut { values: Vec<f32>, labels: Vec<String> },
    Scatter { points: Vec<(f32, f32)> },
}

pub struct Chart {
    kind: ChartKind,
}

impl Chart {
    pub fn new(kind: ChartKind) -> Self {
        Self { kind }
    }

    pub fn view(&self) -> Element<'_, ChartMessage> {
        match &self.kind {
            ChartKind::Bar { values, labels } => self.bar_chart(values, labels),
            ChartKind::Donut { values, labels } => self.donut_chart(values, labels),
            ChartKind::Scatter { points } => self.scatter_chart(points),
        }
    }

    fn bar_chart(&self, values: &[f32], labels: &[String]) -> Element<'_, ChartMessage> {
        // Simple bar chart: show values as text for now.
        // Canvas-based rendering deferred — this is a text-based placeholder
        // that shows the data.
        let mut col = column![text("Cost by Session").size(16)].spacing(4);
        for (i, (val, _label)) in values.iter().zip(labels.iter()).enumerate() {
            let bar = format!("{}: ${:.3} {}", "#", val, "█".repeat((*val as usize).max(1)));
            col = col.push(text(bar).size(12));
            if i > 10 {
                break;
            }
        }
        container(col).padding(8).into()
    }

    fn donut_chart(&self, values: &[f32], labels: &[String]) -> Element<'_, ChartMessage> {
        let total: f32 = values.iter().sum();
        let mut col = column![text("Provider Breakdown").size(16)].spacing(4);
        for (val, label) in values.iter().zip(labels.iter()) {
            let pct = if total > 0.0 { (val / total) * 100.0 } else { 0.0 };
            col = col.push(text(format!("{}: {:.1}%", label, pct)).size(12));
        }
        container(col).padding(8).into()
    }

    fn scatter_chart(&self, points: &[(f32, f32)]) -> Element<'_, ChartMessage> {
        let mut col = column![text("Tokens vs Cost").size(16)].spacing(4);
        for (i, (x, y)) in points.iter().enumerate() {
            col = col.push(text(format!("Point {}: ({:.1}, {:.3})", i, x, y)).size(12));
            if i > 10 {
                break;
            }
        }
        container(col).padding(8).into()
    }
}
