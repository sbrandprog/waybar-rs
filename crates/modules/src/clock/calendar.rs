use std::cell::{Cell, RefCell};
use std::rc::Rc;

use chrono::{Datelike, Duration, Locale, NaiveDate, Weekday};
use gtk::prelude::*;
use serde_json::Value;
use tracing::warn;

use waybar_core::widgets::set_click_cursor;

use super::{ClockTime, TzSpec, now_in, parse_tz};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WeekStart {
    Monday,
    Sunday,
}

impl WeekStart {
    fn weekday(self) -> Weekday {
        match self {
            WeekStart::Monday => Weekday::Mon,
            WeekStart::Sunday => Weekday::Sun,
        }
    }
}

pub(super) struct CalendarConfig {
    week_start: WeekStart,
    week_numbers: bool,
    show_weekdays: bool,
    other_months: bool,
    tz: TzSpec,
}

impl CalendarConfig {
    pub(super) fn parse(raw: Option<&Value>, module_tz: TzSpec, ctx: &str) -> Option<Rc<Self>> {
        let raw = raw?;
        let object = match raw {
            Value::Bool(true) => None,
            Value::Object(map) => Some(map),
            Value::Bool(false) | Value::Null => return None,
            other => {
                warn!("{ctx}: invalid `calendar` option ({other}); calendar disabled");
                return None;
            }
        };
        let get = |key: &str| object.and_then(|map| map.get(key));

        let week_start = match get("week-start").and_then(Value::as_str) {
            None => WeekStart::Monday,
            Some(s) if s.eq_ignore_ascii_case("monday") => WeekStart::Monday,
            Some(s) if s.eq_ignore_ascii_case("sunday") => WeekStart::Sunday,
            Some(s) => {
                warn!("{ctx}: unknown calendar `week-start` `{s}`, using monday");
                WeekStart::Monday
            }
        };

        let tz = match get("timezone").and_then(Value::as_str) {
            None => module_tz,
            Some(s) if s.trim().is_empty() => module_tz,
            Some(s) => match parse_tz(Some(s)) {
                Ok(tz) => tz,
                Err(_) => {
                    warn!("{ctx}: unknown calendar timezone `{s}`, using module timezone");
                    module_tz
                }
            },
        };

        Some(Rc::new(CalendarConfig {
            week_start,
            week_numbers: get("week-numbers")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            show_weekdays: get("show-weekdays")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            other_months: get("other-months")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tz,
        }))
    }
}

fn today(tz: TzSpec) -> NaiveDate {
    match now_in(tz) {
        ClockTime::Local(dt) => dt.date_naive(),
        ClockTime::Named(dt) => dt.date_naive(),
    }
}

fn month_title(year: i32, month: u32, locale: Option<Locale>) -> String {
    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return String::new();
    };
    match locale {
        Some(locale) => first.format_localized("%B %Y", locale).to_string(),
        None => first.format("%B %Y").to_string(),
    }
}

fn weekday_labels(week_start: WeekStart, locale: Option<Locale>) -> [String; 7] {
    let Some(monday) = NaiveDate::from_ymd_opt(2024, 1, 1) else {
        return std::array::from_fn(|_| String::new());
    };
    let start = week_start.weekday().num_days_from_monday();
    std::array::from_fn(|i| {
        let date = monday + Duration::days(((start + i as u32) % 7) as i64);
        match locale {
            Some(locale) => date.format_localized("%a", locale).to_string(),
            None => date.format("%a").to_string(),
        }
    })
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let start = NaiveDate::from_ymd_opt(year, month, 1);
    let next = NaiveDate::from_ymd_opt(next_year, next_month, 1);
    match (start, next) {
        (Some(start), Some(next)) => (next - start).num_days() as u32,
        _ => 0,
    }
}

fn month_grid(
    year: i32,
    month: u32,
    week_start: WeekStart,
    other_months: bool,
) -> Vec<Vec<Option<NaiveDate>>> {
    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return Vec::new();
    };
    let days = days_in_month(year, month);
    let Some(last) = NaiveDate::from_ymd_opt(year, month, days) else {
        return Vec::new();
    };

    let leading = first.weekday().days_since(week_start.weekday());
    let trailing = 6 - last.weekday().days_since(week_start.weekday());
    let weeks = (leading + days + trailing).div_ceil(7);

    let mut cursor = first - Duration::days(i64::from(leading));
    let mut grid = Vec::with_capacity(weeks as usize);
    for _ in 0..weeks {
        let mut row = Vec::with_capacity(7);
        for _ in 0..7 {
            let in_month = cursor.year() == year && cursor.month() == month;
            row.push((in_month || other_months).then_some(cursor));
            cursor += Duration::days(1);
        }
        grid.push(row);
    }
    grid
}

fn add_months(date: NaiveDate, delta: i32) -> NaiveDate {
    let total = date.year() * 12 + date.month0() as i32 + delta;
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) as u32 + 1;
    NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(date)
}

pub(super) struct CalendarView {
    popover: gtk::Popover,
    title: gtk::Label,
    grid: gtk::Grid,
    cfg: Rc<CalendarConfig>,
    locale: Option<Locale>,
    shown: Cell<NaiveDate>,
    selected: Cell<NaiveDate>,
    selected_button: RefCell<Option<gtk::Button>>,
}

impl CalendarView {
    pub(super) fn new(
        parent: &impl IsA<gtk::Widget>,
        cfg: Rc<CalendarConfig>,
        locale: Option<Locale>,
    ) -> Rc<Self> {
        let title = gtk::Label::new(None);
        title.add_css_class("clock-title");

        let prev = gtk::Button::with_label("‹");
        prev.add_css_class("clock-nav");
        set_click_cursor(&prev);
        let next = gtk::Button::with_label("›");
        next.add_css_class("clock-nav");
        set_click_cursor(&next);

        let nav = gtk::CenterBox::new();
        nav.add_css_class("clock-nav-box");
        nav.set_start_widget(Some(&prev));
        nav.set_center_widget(Some(&title));
        nav.set_end_widget(Some(&next));

        let grid = gtk::Grid::new();
        grid.add_css_class("clock-grid");
        grid.set_column_homogeneous(true);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("clock-calendar");
        root.append(&nav);
        root.append(&grid);

        let popover = gtk::Popover::new();
        popover.add_css_class("clock-menu");
        popover.set_child(Some(&root));
        popover.set_parent(parent);

        let now = today(cfg.tz);
        let shown = now.with_day(1).unwrap_or(now);
        let view = Rc::new(CalendarView {
            popover,
            title,
            grid,
            cfg,
            locale,
            shown: Cell::new(shown),
            selected: Cell::new(now),
            selected_button: RefCell::new(None),
        });

        for (button, delta) in [(&prev, -1), (&next, 1)] {
            let weak = Rc::downgrade(&view);
            button.connect_clicked(move |_| {
                if let Some(view) = weak.upgrade() {
                    view.shift_month(delta);
                }
            });
        }

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        {
            let weak = Rc::downgrade(&view);
            scroll.connect_scroll(move |_, _dx, dy| {
                if let Some(view) = weak.upgrade() {
                    if dy < 0.0 {
                        view.shift_month(-1);
                    } else if dy > 0.0 {
                        view.shift_month(1);
                    }
                }
                gtk::glib::Propagation::Proceed
            });
        }
        root.add_controller(scroll);

        view.rebuild();
        view
    }

    pub(super) fn popup(self: &Rc<Self>) {
        let now = today(self.cfg.tz);
        self.shown.set(now.with_day(1).unwrap_or(now));
        self.rebuild();
        self.popover.popup();
    }

    pub(super) fn popdown(self: &Rc<Self>) {
        self.popover.popdown();
    }

    pub(super) fn toggle(self: &Rc<Self>) {
        if self.popover.is_visible() {
            self.popdown();
        } else {
            self.popup();
        }
    }

    fn shift_month(self: &Rc<Self>, delta: i32) {
        self.shown.set(add_months(self.shown.get(), delta));
        self.rebuild();
    }

    fn rebuild(self: &Rc<Self>) {
        let shown = self.shown.get();
        self.title
            .set_text(&month_title(shown.year(), shown.month(), self.locale));

        while let Some(child) = self.grid.first_child() {
            child.unparent();
        }
        self.selected_button.borrow_mut().take();

        let today = today(self.cfg.tz);
        let first_column = i32::from(self.cfg.week_numbers);

        if self.cfg.week_numbers {
            let header = gtk::Label::new(None);
            header.add_css_class("clock-week-number");
            self.grid.attach(&header, 0, 0, 1, 1);
        }
        if self.cfg.show_weekdays {
            for (i, name) in weekday_labels(self.cfg.week_start, self.locale)
                .iter()
                .enumerate()
            {
                let label = gtk::Label::new(Some(name));
                label.add_css_class("clock-weekday");
                self.grid.attach(&label, first_column + i as i32, 0, 1, 1);
            }
        }

        let first_row = i32::from(self.cfg.show_weekdays || self.cfg.week_numbers);
        let weeks = month_grid(
            shown.year(),
            shown.month(),
            self.cfg.week_start,
            self.cfg.other_months,
        );
        for (r, week) in weeks.iter().enumerate() {
            let row = first_row + r as i32;
            if self.cfg.week_numbers
                && let Some(first) = week.iter().flatten().next()
            {
                let number = gtk::Label::new(Some(&first.iso_week().week().to_string()));
                number.add_css_class("clock-week-number");
                self.grid.attach(&number, 0, row, 1, 1);
            }
            for (c, day) in week.iter().enumerate() {
                let Some(date) = day else {
                    continue;
                };
                let date = *date;
                let button = gtk::Button::with_label(&date.day().to_string());
                button.add_css_class("clock-day");
                set_click_cursor(&button);
                if date.month() != shown.month() || date.year() != shown.year() {
                    button.add_css_class("other-month");
                }
                if date == today {
                    button.add_css_class("today");
                }
                if date == self.selected.get() {
                    button.add_css_class("selected");
                    *self.selected_button.borrow_mut() = Some(button.clone());
                }

                let weak = Rc::downgrade(self);
                let clicked = button.clone();
                button.connect_clicked(move |_| {
                    let Some(view) = weak.upgrade() else {
                        return;
                    };
                    view.selected.set(date);
                    if let Some(previous) =
                        view.selected_button.borrow_mut().replace(clicked.clone())
                    {
                        previous.remove_css_class("selected");
                    }
                    clicked.add_css_class("selected");
                });

                self.grid
                    .attach(&button, first_column + c as i32, row, 1, 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn monday_grid_for_march_2024() {
        let grid = month_grid(2024, 3, WeekStart::Monday, false);
        assert_eq!(grid.len(), 5);
        assert_eq!(grid[0][0], None);
        assert_eq!(grid[0][4], Some(date(2024, 3, 1)));
        assert_eq!(grid[0][6], Some(date(2024, 3, 3)));
        assert_eq!(grid[4][6], Some(date(2024, 3, 31)));
    }

    #[test]
    fn sunday_grid_shifts_the_first_column() {
        let grid = month_grid(2024, 3, WeekStart::Sunday, false);
        assert_eq!(grid[0][5], Some(date(2024, 3, 1)));
        assert_eq!(grid[0][0], None);
    }

    #[test]
    fn february_2024_is_a_leap_month() {
        let grid = month_grid(2024, 2, WeekStart::Monday, false);
        let days = grid
            .iter()
            .flatten()
            .filter(|d| d.is_some_and(|d| d.month() == 2))
            .count();
        assert_eq!(days, 29);
        assert_eq!(grid.iter().flatten().count(), grid.len() * 7);
    }

    #[test]
    fn other_months_fills_the_edges() {
        let filled = month_grid(2024, 3, WeekStart::Monday, true);
        assert_eq!(filled[0][0], Some(date(2024, 2, 26)));
        assert_eq!(filled.iter().flatten().filter(|d| d.is_none()).count(), 0);

        let empty = month_grid(2024, 3, WeekStart::Monday, false);
        assert_eq!(empty[0][0], None);
    }

    #[test]
    fn clamps_to_days_in_month() {
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2024, 4), 30);
        assert_eq!(days_in_month(2024, 12), 31);
    }

    #[test]
    fn localized_titles_and_weekdays() {
        assert_eq!(month_title(2024, 3, None), "March 2024");
        let ru = Some("ru_RU".parse::<Locale>().unwrap());
        assert!(month_title(2024, 3, ru).to_lowercase().contains("март"));

        let monday = weekday_labels(WeekStart::Monday, None);
        assert_eq!(monday[0], "Mon");
        assert_eq!(monday[6], "Sun");
        let sunday = weekday_labels(WeekStart::Sunday, None);
        assert_eq!(sunday[0], "Sun");
        assert_eq!(sunday[1], "Mon");
    }

    #[test]
    fn adds_months_across_year_boundaries() {
        assert_eq!(add_months(date(2024, 1, 1), -1), date(2023, 12, 1));
        assert_eq!(add_months(date(2024, 12, 1), 1), date(2025, 1, 1));
        assert_eq!(add_months(date(2024, 6, 1), 0), date(2024, 6, 1));
        assert_eq!(add_months(date(2024, 6, 1), 18), date(2025, 12, 1));
    }

    #[test]
    fn parses_calendar_defaults_and_overrides() {
        let cfg =
            CalendarConfig::parse(Some(&serde_json::json!(true)), TzSpec::Local, "clock").unwrap();
        assert_eq!(cfg.week_start, WeekStart::Monday);
        assert!(!cfg.week_numbers);
        assert!(cfg.show_weekdays);
        assert!(!cfg.other_months);

        let cfg = CalendarConfig::parse(
            Some(&serde_json::json!({
                "week-start": "sunday",
                "week-numbers": true,
                "show-weekdays": false,
                "other-months": true,
            })),
            TzSpec::Local,
            "clock",
        )
        .unwrap();
        assert_eq!(cfg.week_start, WeekStart::Sunday);
        assert!(cfg.week_numbers);
        assert!(!cfg.show_weekdays);
        assert!(cfg.other_months);
    }

    #[test]
    fn disabled_calendar_is_none() {
        assert!(CalendarConfig::parse(None, TzSpec::Local, "clock").is_none());
        assert!(
            CalendarConfig::parse(Some(&serde_json::json!(false)), TzSpec::Local, "clock")
                .is_none()
        );
    }

    #[test]
    fn empty_calendar_timezone_inherits_module() {
        let named = "Europe/Moscow".parse::<chrono_tz::Tz>().unwrap();
        let cfg = CalendarConfig::parse(
            Some(&serde_json::json!({"timezone": ""})),
            TzSpec::Named(named),
            "clock",
        )
        .unwrap();
        assert!(matches!(cfg.tz, TzSpec::Named(_)));

        let utc = "UTC".parse::<chrono_tz::Tz>().unwrap();
        let cfg = CalendarConfig::parse(
            Some(&serde_json::json!({"timezone": "UTC"})),
            TzSpec::Named(named),
            "clock",
        )
        .unwrap();
        assert!(matches!(cfg.tz, TzSpec::Named(tz) if tz == utc));
    }

    #[test]
    fn unknown_week_start_falls_back() {
        let cfg = CalendarConfig::parse(
            Some(&serde_json::json!({"week-start": "funday"})),
            TzSpec::Local,
            "clock",
        )
        .unwrap();
        assert_eq!(cfg.week_start, WeekStart::Monday);
    }
}
