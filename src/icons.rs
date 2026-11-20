use egui::{Button, Image, WidgetText};

pub enum Icon {
    Add,
    Backup,
    Cancel,
    Clear,
    Delete,
    Deselect,
    Down,
    Edit,
    Export,
    Folder,
    Refresh,
    Save,
    Select,
    Up,
}

impl Icon {
    pub fn button(self, label: impl Into<WidgetText>) -> Button<'static> {
        let source = match self {
            Self::Add => egui::include_image!("../assets/icons/add.svg"),
            Self::Backup => egui::include_image!("../assets/icons/backup.svg"),
            Self::Cancel => egui::include_image!("../assets/icons/cancel.svg"),
            Self::Clear => egui::include_image!("../assets/icons/clear.svg"),
            Self::Delete => egui::include_image!("../assets/icons/delete.svg"),
            Self::Deselect => egui::include_image!("../assets/icons/deselect.svg"),
            Self::Down => egui::include_image!("../assets/icons/down.svg"),
            Self::Edit => egui::include_image!("../assets/icons/edit.svg"),
            Self::Export => egui::include_image!("../assets/icons/export.svg"),
            Self::Folder => egui::include_image!("../assets/icons/folder.svg"),
            Self::Refresh => egui::include_image!("../assets/icons/refresh.svg"),
            Self::Save => egui::include_image!("../assets/icons/save.svg"),
            Self::Select => egui::include_image!("../assets/icons/select.svg"),
            Self::Up => egui::include_image!("../assets/icons/up.svg"),
        };
        let image = Image::new(source).fit_to_exact_size(egui::Vec2::splat(15.0));
        Button::image_and_text(image, label).image_tint_follows_text_color(true)
    }
}
