//! Schema migrations with `sea-orm-migration`. They run at startup, in the
//! factory that opens the connection (see `lib.rs`); each runs once
//! (tracked in `seaql_migrations`).

use sea_orm_migration::prelude::*;

/// Every migration, in order.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(CreateBlog)]
    }
}

#[derive(DeriveMigrationName)]
struct CreateBlog;

#[async_trait::async_trait]
impl MigrationTrait for CreateBlog {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Authors::Table)
                    .col(
                        ColumnDef::new(Authors::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Authors::Name)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(Posts::Table)
                    .col(
                        ColumnDef::new(Posts::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Posts::AuthorId).integer().not_null())
                    .col(ColumnDef::new(Posts::Title).string().not_null())
                    .col(ColumnDef::new(Posts::Body).text().not_null())
                    .col(
                        ColumnDef::new(Posts::Published)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_posts_author")
                            .from(Posts::Table, Posts::AuthorId)
                            .to(Authors::Table, Authors::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("posts_author_id")
                    .table(Posts::Table)
                    .col(Posts::AuthorId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Posts::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Authors::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Authors {
    Table,
    Id,
    Name,
}

#[derive(DeriveIden)]
enum Posts {
    Table,
    Id,
    AuthorId,
    Title,
    Body,
    Published,
}
