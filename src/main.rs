use std::vec;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    let v: Vec<i32> = vec![1, 2, 3, 4, 5, 6];
    let vv = std::rc::Rc::new(slint::VecModel::from(v));
    ui.set_values(vv.into());
    ui.set_cols(3);
    ui.set_rows(2);
    ui.set_selected_x(1);
    ui.set_selected_y(1);

    ui.run()
}
