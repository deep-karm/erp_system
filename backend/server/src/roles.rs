use serde::Deserialize;
use axum::{http::StatusCode, extract, Json};
use sqlx::PgPool;
use crate::logger::{LogType, admin_logger};


#[derive(Deserialize)]
pub struct CreateRole {
	role_: String
}

#[derive(sqlx::FromRow)]
pub struct RoleDef {
	id: i32,
	role_: String, 
}

pub async fn create_role(
	extract::State(pool) : extract::State<PgPool>,
	Json(payload) : Json<CreateRole>
) -> Result<StatusCode, StatusCode> {

	let role = payload.role_;
	let insert_into_role = 
		sqlx::query("insert into role_defs (role_) values ($1)")
		.bind(role)
		.execute(&pool)
		.await;

	if let Err(e) = insert_into_role {
		admin_logger(LogType::Error, &format!("Error insert into role_defs: {}", e), None)
			.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}

	return Ok(StatusCode::CREATED);
}

pub async fn get_all_roles(
	extract::State(pool) : extract::State<PgPool>
) -> Result<(StatusCode, Json<Vec<String>>), StatusCode> {
	let query : Result<Vec<RoleDef>, _> = sqlx::query_as("select * from role_defs")
		.fetch_all(&pool)
		.await;

	if let Err(e) = query {
		admin_logger(LogType::Error, &format!("Error in get_current_roles : {}", e), None)
			.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}
	let query = query.unwrap()
		.iter()
		.map(|x| x.role_.to_owned())
		.collect::<Vec<_>>();

	return Ok((StatusCode::OK, Json(query)));
}