use storage_manager::catalog::{Catalog, init_catalog, load_catalog};

pub fn initialize_catalog() -> Catalog {
    init_catalog();
    load_catalog()
}