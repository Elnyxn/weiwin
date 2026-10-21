//! Native popup interaction with the same typography and surfaces as the panel.
#![allow(unsafe_op_in_unsafe_fn)]
use std::{mem, ptr};
use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Gdi::*,
    UI::{Controls::*, WindowsAndMessaging::*},
};

struct Item {
    text: Vec<u16>,
    font: HFONT,
    dpi: u32,
    checked: bool,
}
fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

pub unsafe fn popup(
    hwnd: HWND,
    x: i32,
    y: i32,
    choices: &[(usize, String, bool)],
    font: HFONT,
    dpi: u32,
) -> usize {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return 0;
    }
    let items = choices
        .iter()
        .map(|(_, text, checked)| {
            Box::new(Item {
                text: text.encode_utf16().chain(Some(0)).collect(),
                font,
                dpi,
                checked: *checked,
            })
        })
        .collect::<Vec<_>>();
    for ((id, _, checked), item) in choices.iter().zip(&items) {
        let mut info: MENUITEMINFOW = mem::zeroed();
        info.cbSize = mem::size_of::<MENUITEMINFOW>() as u32;
        info.fMask = MIIM_FTYPE | MIIM_ID | MIIM_DATA | MIIM_STRING | MIIM_STATE;
        info.fType = MFT_OWNERDRAW;
        info.wID = *id as u32;
        info.fState = if *checked { MFS_CHECKED } else { MFS_UNCHECKED };
        info.dwItemData = (&**item as *const Item) as usize;
        info.dwTypeData = item.text.as_ptr() as *mut _;
        info.cch = (item.text.len() - 1) as u32;
        InsertMenuItemW(menu, u32::MAX, 1, &info);
    }
    let chosen = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
        x,
        y,
        0,
        hwnd,
        ptr::null(),
    );
    DestroyMenu(menu);
    PostMessageW(hwnd, WM_NULL, 0, 0);
    chosen as usize
}

pub unsafe fn measure(hwnd: HWND, value: *mut MEASUREITEMSTRUCT) -> bool {
    let value = &mut *value;
    if value.CtlType != ODT_MENU || value.itemData == 0 {
        return false;
    }
    let item = &*(value.itemData as *const Item);
    let dc = GetDC(hwnd);
    let previous = SelectObject(dc, item.font);
    let mut size = mem::zeroed();
    GetTextExtentPoint32W(
        dc,
        item.text.as_ptr(),
        (item.text.len() - 1) as i32,
        &mut size,
    );
    SelectObject(dc, previous);
    ReleaseDC(hwnd, dc);
    value.itemHeight = scale(32, item.dpi) as u32;
    value.itemWidth =
        (size.cx + scale(54, item.dpi)).clamp(scale(170, item.dpi), scale(410, item.dpi)) as u32;
    true
}

pub unsafe fn draw(value: &DRAWITEMSTRUCT) -> bool {
    if value.CtlType != ODT_MENU || value.itemData == 0 {
        return false;
    }
    let item = &*(value.itemData as *const Item);
    let selected = value.itemState & ODS_SELECTED != 0;
    let brush = CreateSolidBrush(0x00fdfdfd);
    FillRect(value.hDC, &value.rcItem, brush);
    DeleteObject(brush);
    crate::view::menu_row(value.hDC, item.dpi, &value.rcItem, selected, item.checked);
    SetBkMode(value.hDC, TRANSPARENT as i32);
    SetTextColor(value.hDC, 0x00382f27);
    let previous = SelectObject(value.hDC, item.font);
    let mut rect = value.rcItem;
    rect.left += scale(34, item.dpi);
    rect.right -= scale(14, item.dpi);
    DrawTextW(
        value.hDC,
        item.text.as_ptr(),
        (item.text.len() - 1) as i32,
        &mut rect,
        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    SelectObject(value.hDC, previous);
    true
}
