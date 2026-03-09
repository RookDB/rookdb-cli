use storage_manager::catalog::{init_catalog, load_catalog, show_databases};

pub fn initialize_and_show_catalog() {
    init_catalog();
    let catalog = load_catalog();
    show_databases(&catalog);
}