use wobble::types::{Block, Transaction};

fn main() {
    println!(
        "wobble node scaffold loaded: {} block type, {} transaction type",
        std::any::type_name::<Block>(),
        std::any::type_name::<Transaction>(),
    );
}
