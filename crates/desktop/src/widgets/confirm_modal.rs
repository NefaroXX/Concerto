use iced::widget::{button, column, container, row, text};
use iced::{Element, Length};

#[derive(Debug, Clone)]
pub struct ConfirmModal {
    pub title: String,
    pub body: String,
    pub confirm_label: String,
    pub danger: bool,
}

#[derive(Debug, Clone)]
pub enum ConfirmMessage {
    Confirm,
    Cancel,
}

impl ConfirmModal {
    pub fn delete(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            confirm_label: "Delete".into(),
            danger: true,
        }
    }

    pub fn confirm(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            confirm_label: "Confirm".into(),
            danger: false,
        }
    }

    pub fn view(&self) -> Element<'_, ConfirmMessage> {
        let title = text(&self.title).size(18);
        let body = text(&self.body).size(14);

        let confirm_btn = if self.danger {
            button(text(&self.confirm_label)).style(crate::ui::button::danger)
        } else {
            button(text(&self.confirm_label)).style(crate::ui::button::primary)
        }
        .on_press(ConfirmMessage::Confirm);

        let cancel_btn = button(text("Cancel"))
            .style(crate::ui::button::secondary)
            .on_press(ConfirmMessage::Cancel);

        let content = column![title, body, row![confirm_btn, cancel_btn].spacing(10).padding(10),]
            .spacing(10)
            .padding(20)
            .width(Length::Fill);

        container(content)
            .width(400)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(crate::ui::container::modal)
            .into()
    }
}
