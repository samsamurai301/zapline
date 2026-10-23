//! The schema as Diesel sees it. `diesel print-schema` writes this file
//! from the database; it is checked in so the build needs no database.

diesel::table! {
    products (id) {
        id -> Integer,
        sku -> Text,
        name -> Text,
        price_cents -> BigInt,
        stock -> Integer,
    }
}

diesel::table! {
    orders (id) {
        id -> Integer,
        customer -> Text,
        total_cents -> BigInt,
        created_at -> Text,
    }
}

diesel::table! {
    order_lines (id) {
        id -> Integer,
        order_id -> Integer,
        product_id -> Integer,
        quantity -> Integer,
        unit_price_cents -> BigInt,
    }
}

diesel::joinable!(order_lines -> orders (order_id));
diesel::joinable!(order_lines -> products (product_id));
diesel::allow_tables_to_appear_in_same_query!(products, orders, order_lines);
