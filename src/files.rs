//! File browser state is owned by its SSH tab, independently of terminal focus.
use std::{collections::BTreeSet, path::PathBuf};
use iced::{Element, Length, widget::{button, column, container, mouse_area, row, scrollable, text, text_input}};
use crate::{sftp, ACCENT, BG, DIM, FG, SIDEBAR, flat_button, primary_button};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side { Local, Remote }
#[derive(Debug, Clone)]
pub enum Message {
    Click(Side, usize), Open(Side, usize), Context(Side, usize), Transfer(Side),
    Up(Side), Refresh, Path(Side, String), Go(Side), Answer(bool), Dismiss,
}
#[derive(Default)]
pub struct Selection { pub indices: BTreeSet<usize>, anchor: Option<usize> }
impl Selection {
    pub fn click(&mut self, index: usize, modifiers: iced::keyboard::Modifiers) {
        if modifiers.shift() {
            let anchor = self.anchor.unwrap_or(index);
            self.indices = (anchor.min(index)..=anchor.max(index)).collect();
        } else if modifiers.control() || modifiers.logo() {
            if !self.indices.remove(&index) { self.indices.insert(index); }
            self.anchor = Some(index);
        } else { self.indices = [index].into(); self.anchor = Some(index); }
    }
}
pub struct Ui {
    pub window: Option<iced::window::Id>,
    pub path: String, pub entries: Vec<sftp::Entry>,
    pub local: PathBuf, pub local_entries: Vec<sftp::Entry>,
    pub local_input: String, pub remote_input: String,
    pub selected_local: Selection, pub selected_remote: Selection,
    pub menu: Option<(Side, usize)>, pub conflict: Option<sftp::Conflict>,
    pub status: String, pub busy: bool, pub remaining: usize,
}
impl Default for Ui {
    fn default() -> Self {
        let local = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        let mut ui = Self { window: None, path: String::new(), entries: vec![], local_input: local.display().to_string(), remote_input: String::new(), local,
            local_entries: vec![], selected_local: Selection::default(), selected_remote: Selection::default(), menu: None, conflict: None, status: String::new(), busy: false, remaining: 0 };
        ui.refresh_local(); ui
    }
}
impl Ui {
    pub fn refresh_local(&mut self) {
        match std::fs::read_dir(&self.local).and_then(|entries| entries.map(|e| {
            let e = e?; let m = e.metadata()?;
            Ok(sftp::Entry { name: e.file_name().to_string_lossy().into_owned(), is_dir: m.is_dir(), is_symlink: e.file_type()?.is_symlink(), size: m.len() })
        }).collect::<Result<Vec<_>, std::io::Error>>()) {
            Ok(mut entries) => { entries.sort_by(|a,b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))); self.local_entries = entries; self.selected_local = Selection::default(); }
            Err(e) => {
                self.local_entries.clear();
                self.selected_local = Selection::default();
                self.status = format!("读取本地目录失败: {e}");
            },
        }
        self.local_input = self.local.display().to_string();
    }
    pub fn dismiss_conflict(&mut self) {
        if let Some(c) = self.conflict.take() { if let Some(tx) = c.reply.lock().unwrap().take() { let _ = tx.send(false); } }
    }
}
pub fn view(ui: &Ui) -> Element<'_, Message> {
    let mut content = column![text("单击选择 · 双击进入目录 · Shift 范围选择 · Ctrl / Command 多选 · 右键上传/下载").size(12).color(DIM),
        row![pane(ui, Side::Local), pane(ui, Side::Remote)].spacing(12).height(Length::Fill)].spacing(10);
    if let Some(c) = &ui.conflict {
        content = content.push(container(column![text("目标已存在，是否覆盖文件 / 合并目录？").color(FG), text(&c.path).size(13).color(ACCENT),
            row![button("覆盖").on_press(Message::Answer(true)).style(primary_button), button("跳过此项").on_press(Message::Answer(false)).style(flat_button)].spacing(12)].spacing(8)).padding(12));
    }
    content = content.push(text(&ui.status).size(12).color(FG));
    container(content).padding(14).height(Length::Fill).style(|_| container::Style { background: Some(BG.into()), ..Default::default() }).into()
}
fn pane(ui: &Ui, side: Side) -> Element<'_, Message> {
    let (entries, selection, path, title) = if side == Side::Local { (&ui.local_entries, &ui.selected_local, &ui.local_input, "本地") } else { (&ui.entries, &ui.selected_remote, &ui.remote_input, "远程") };
    let mut list = column![].spacing(2);
    for (i,e) in entries.iter().enumerate() {
        let selected = selection.indices.contains(&i);
        let label = row![text(format!("{} {}", if e.is_symlink { "↗" } else if e.is_dir { "▸" } else { " " }, e.name)).size(14).color(if e.is_dir { ACCENT } else { FG }).width(Length::Fill), text(if e.is_dir { String::new() } else { sftp::human_size(e.size) }).size(12).color(DIM)].spacing(8);
        let item = container(label).padding([5,8]).width(Length::Fill).style(move |_| container::Style { background: selected.then_some(iced::Color::from_rgba(0.65,0.89,0.18,0.2).into()), ..Default::default() });
        list = list.push(mouse_area(item).on_press(Message::Click(side,i)).on_double_click(Message::Open(side,i)).on_right_press(Message::Context(side,i)));
        if ui.menu == Some((side,i)) {
            list = list.push(container(row![button(text(format!("{}所选 {} 项",if side == Side::Local { "上传" } else { "下载" },selection.indices.len()))).on_press_maybe((!ui.busy && !selection.indices.is_empty()).then_some(Message::Transfer(side))).style(primary_button), button("关闭菜单").on_press(Message::Dismiss)].spacing(8)).padding(8));
        }
    }
    let pane = column![text(title).size(16).color(FG), row![button("↑").on_press(Message::Up(side)), text_input("目录路径",path).on_input(move |v| Message::Path(side,v)).on_submit(Message::Go(side)), button("刷新").on_press(Message::Refresh)].spacing(6), scrollable(list).height(Length::Fill)].spacing(8);
    container(pane).padding(10).width(Length::Fill).height(Length::Fill).style(|_| container::Style { background: Some(SIDEBAR.into()), ..Default::default() }).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::Modifiers;

    #[test]
    fn selection_supports_range_toggle_and_plain_click() {
        let mut selection = Selection::default();
        selection.click(2, Modifiers::empty());
        selection.click(5, Modifiers::SHIFT);
        assert_eq!(selection.indices, [2,3,4,5].into());
        selection.click(3, Modifiers::CTRL);
        selection.click(7, Modifiers::LOGO);
        assert_eq!(selection.indices, [2,4,5,7].into());
        selection.click(1, Modifiers::empty());
        assert_eq!(selection.indices, [1].into());
        selection.click(0, Modifiers::SHIFT);
        assert_eq!(selection.indices, [0,1].into());
    }

    #[test]
    fn closing_confirmation_does_not_approve_overwrite() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut ui = Ui::default();
        ui.conflict = Some(sftp::Conflict { path: "existing.txt".into(), reply: std::sync::Arc::new(std::sync::Mutex::new(Some(tx))) });
        ui.dismiss_conflict();
        assert_eq!(rx.try_recv(), Ok(false));
    }
}
