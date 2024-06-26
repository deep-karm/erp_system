use axum::{Json, http::StatusCode, extract};
use serde::{Serialize, Deserialize};
use serde_json::Map;
use sqlx::FromRow;
use crate::{callbacks::send_task, db_types::Ticket, process::{read_process_data, Process}};
use std::collections::VecDeque;
use crate::{utils, logger::{LogType, log, admin_logger}};
use crate::notif_handler::{Ping, ping_notifier};

#[derive(Eq, PartialEq, Clone, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Event {Initiate, Approve, Notify, NonBlockingTask, BlockingTask, Complete}
#[derive(Debug)]
pub enum TicketStatus {Open, Closed, Rejected}

#[derive(Debug)]
pub enum NewUserTicketType {ApproveRequest, Notify, Completion}
#[derive(Debug)]
pub struct NewUserTicket {
	pub type_ : NewUserTicketType,
	pub ticket_id: i32,
	pub node: i32,
	pub username: Option<String>
}
#[derive(Debug)]
pub struct SingleExecState {
	pub status: TicketStatus,
	pub new_ticket : Option<NewUserTicket>,
	pub completable_steps : Vec<i32>,
}
#[derive(Debug)]
pub enum ExecuteErr {InvalidTicket, FailedToExecute, InvalidEvent, FailedToReadProcessData, FailedToLog, FailedToNotify, FailedToExecuteCallback}
#[derive(Serialize, Deserialize)]
pub struct CreateTicket {
	pub process_id: String,
	pub owner_id: uuid::Uuid,
	pub owner_name: String,
	pub is_public: bool,
	pub data: Option<Map<String, serde_json::Value>>
}

#[derive(Serialize, Deserialize)]
pub struct UpdateTicket {
	pub ticket_id: i32,
	pub user_id: uuid::Uuid,
	pub	status: bool,
	pub node: i32,
	pub data: Option<Map<String, serde_json::Value>>
}
#[derive(Serialize, Deserialize, FromRow)]
pub struct UserIdQueryRes {
	userid: uuid::Uuid
}
#[derive(Serialize, Deserialize)]
pub struct UserTickets {
	current_tickets: Vec<CurrentTicket>,
	own_tickets: Vec<OwnTicket>
}
#[derive(Serialize, Deserialize, FromRow)]
pub struct CurrentTicket {
	type_: String,
	ticketid: i32,
	active: bool,
	node_number: i32,
	process_id: String,
	owner_name: String
}
#[derive(Serialize, Deserialize, FromRow)]
pub struct OwnTicket {
	pub id: i32,
	pub process_id: String,
	pub is_public: bool,
	pub created_at: chrono::DateTime<chrono::Utc>,
	pub updated_at: chrono::DateTime<chrono::Utc>,
	pub status: String,
}
#[derive(Serialize, Deserialize)]
pub struct GetUserTicketsReq {
	pub userid: String 
}

#[derive(Serialize, FromRow, Deserialize)]
struct Userid {
	userid: uuid::Uuid
}

#[derive(Serialize, FromRow, Deserialize)]
struct Username {
	username: String
}


pub async fn create_ticket(
	extract::State(pool): extract::State<sqlx::PgPool>,
	Json(payload) : Json<CreateTicket>
) -> Result<StatusCode, StatusCode> {
	/*
		1. create a new ticket with the request data and add it to the database;
		2. Fetch the ticket back from the database because we dont know its id from the first step.
		3. Insert a new ticket into user_active_tickets with userid=ticket.owner_id and node=0
		4. Execute the first node of the process (always Event::Initiate)
		5. Add all tickets returned by update_internal
		6. Update the ticket in tickets table with the new values
		7. Commit the transaction
	*/

	let mut tx = pool.begin().await.unwrap();
	let log_id = uuid::Uuid::new_v4();
	let state = payload.data.clone().unwrap_or_default();


	let query = sqlx::query("insert into tickets (owner_id, process_id, log_id, is_public, created_at, updated_at, status, complete, state) values ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
		.bind(payload.owner_id)
		.bind(&payload.process_id)
		.bind(log_id)
		.bind(payload.is_public)
		.bind(chrono::Utc::now())
		.bind(chrono::Utc::now())
		.bind("open")
		// no nodes have been completed at this stage
		.bind(0i32)
		.bind(serde_json::Value::Object(state))
		.execute(&mut *tx)
		.await;


	if let Err(e) = query {
		log(LogType::Error, format!("Error adding ticket: process {} from {}: {}", payload.process_id, payload.owner_id, e), log_id)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}

	// FIXME: should not use log_id for determining the ticket id
	let query: Result<Ticket, _> = sqlx::query_as("select * from tickets where log_id=$1")
		.bind(log_id)
		.fetch_one(&mut *tx)
		.await;

	if let Err(e) = query {
		log(LogType::Error, format!("Error reading ticket id from db: {}", e), log_id)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}
	let mut ticket = query.unwrap();

	log(LogType::Info, format!("Ticket {} created by {}", ticket.id, ticket.owner_id), log_id)?;

	let query = sqlx::query("insert into user_active_tickets (userid, ticketid, active, node_number, type_) values ($1, $2, $3, $4, $5)")
		.bind(payload.owner_id)
		.bind(ticket.id)
		.bind(true)
		.bind(0i32)
		.bind("own")
		.execute(&mut *tx)
		.await;

	if let Err(e) = query {
		log(LogType::Error, format!("Error adding ticket: process {} from {}: {}", payload.process_id, payload.owner_id, e), log_id)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}

	// execute the 1 st node of the ticket (always Event::Initiate)
	// TODO: Initiate Step should also be able to execute callbacks
	let request = &UpdateTicket { ticket_id: ticket.id, user_id: payload.owner_id, status: true, node: 0, data: payload.data };

	let result = update_internal(&mut ticket, request).await;
	if let Err(e) = result {
		log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket.id, e), log_id)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}
	for new_ticket in result.unwrap() {
		match new_ticket.type_ {
			NewUserTicketType::ApproveRequest => {
				// this query should always return 1 row as process always contains valid usernames
				let new_ticket_username = new_ticket.username.unwrap();
				let userid_query: Result<Userid, _> = sqlx::query_as("select userid from users where username=$1")
				.bind(&new_ticket_username)
				.fetch_one(&mut *tx)
				.await;

				if let Err(e) = userid_query {
					log(LogType::Error, format!("Error reading userid from db: {}", e), log_id)?;
					return Err(StatusCode::INTERNAL_SERVER_ERROR);
				}
				let userid = userid_query.unwrap();

				let query = sqlx::query("insert into user_active_tickets (userid, ticketid, active, node_number, type_) values ($1, $2, $3, $4, $5)")
					.bind(userid.userid)
					.bind(new_ticket.ticket_id)
					.bind(true)
					.bind(new_ticket.node)
					.bind("approve")
					.execute(&mut *tx)
					.await;
				if let Err(e) = query {
					log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket.id, e), log_id)?;
					return Err(StatusCode::INTERNAL_SERVER_ERROR);
				}
				log(LogType::Request, format!("Ticket {} approval requested from {}", ticket.id, userid.userid), ticket.log_id)?;
			}
			NewUserTicketType::Notify => {
				// insert a new ticket into notifications table and ping the notifier server

				let owner_name_query: Result<Username, _> = sqlx::query_as("select username from users where userid=$1")
					.bind(ticket.owner_id)
					.fetch_one(&mut *tx)
					.await;

				if let Err(e) = owner_name_query {
					admin_logger(LogType::Error, &format!("failed to get owner name in notification NewUserTicket. create request from {}. Error: {}", ticket.owner_id, e), None)
						.map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;
					return Err(StatusCode::INTERNAL_SERVER_ERROR);
				}
				let message = format!("Ticket created by {}. Process Id: {}", ticket.owner_id, ticket.process_id);

				let notified_username = new_ticket.username.as_ref().unwrap();

				// add notification
				let query = sqlx::query("insert into notifications (userid, message, created_at) values ((select userid from users where username=$1), $2, $3)")
					.bind(notified_username)
					.bind(message)
					.bind(chrono::Utc::now())
					.execute(&mut *tx)
					.await;

				if let Err(e) = query {
					admin_logger(LogType::Error, &format!("failed to add notification in NewUserTicket. create request from {}, Error: {}", ticket.owner_id, e), None)
						.map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;
					return Err(StatusCode::INTERNAL_SERVER_ERROR);
				}
				
				log(LogType::NotificationSuccess, format!("Notification sent to notifier for user {} notified for ticket {}", notified_username, ticket.id), ticket.log_id)
					.map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;
			}
			NewUserTicketType::Completion => {
				// this ticket is always the last in the new_ticket_queue because it requires all other nodes to be executed first
				let query = sqlx::query("update user_active_tickets set active=false where ticketid=$1")
					.bind(ticket.id)
					.execute(&mut *tx)
					.await;
				if let Err(e) = query {
					log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket.id, e), log_id)?;
					return Err(StatusCode::INTERNAL_SERVER_ERROR);
				}
				ticket.status = "closed".to_string();
				log(LogType::Completion, format!("Ticket {} completed", ticket.id), ticket.log_id)?;
			}	
		}
	}

	// update all fields of the ticket
	let query = sqlx::query("update tickets set status=$1, complete=$2, updated_at=$3 where id=$4")
		.bind(&ticket.status)
		.bind(ticket.complete)
		.bind(ticket.updated_at)
		.bind(ticket.id)
		.execute(&mut *tx)
		.await;

	if let Err(e) = query {
		log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket.id, e), log_id)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}


	log(LogType::Info, format!("Ticket {} created successfully", ticket.id), log_id)?;
	// commit the transaction
	if let Err(e) = tx.commit().await {
		log(LogType::Error, format!("Error commiting transaction: {} for pid {}", e, ticket.process_id), log_id)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}

	// notifier server should be pinged only after completing the transaction.
	// failure to ping is recoverable so dont return 500 if it fails
	// when the server is successfully pinged later current notifications will be sent
	if let Err(_e) = ping_notifier(Ping::CollectNew, None).await {
		// FIXME: this might silently fail
		let _ = admin_logger(LogType::FailedToPing, 
			&format!("Failed to ping server for new notification node in NewUserTicket. create request from {}", ticket.owner_id), 
			None
		);
	}
	return Ok(StatusCode::CREATED);
}
#[axum::debug_handler]
pub async fn update_ticket(
	extract::State(pool): extract::State<sqlx::PgPool>,
	Json(payload) : Json<UpdateTicket>,
) -> Result<StatusCode, StatusCode> {
	/*
		INFO: user always receives the ticket from user_active_tickets unless they are the owner of the specific ticket
		1. Set the status of the ticket in user_active_tickets to false.
		2. If the user rejected the ticket (only possible in Event::Approve or Event::BlockingTask) then set the status of the ticket in tickets table to rejected
			and set the status of all tickets with the same ticket_id to false.
		3. If the user accepted the ticket then fetch the complete ticket from tickets table and call update_internal
		4. Add all tickets returned by update_internal
		5. Update the ticket in tickets table with the new values
		6. Commit the transaction
	*/

	let mut tx = pool.begin().await.unwrap();
	let ticket_id = payload.ticket_id;

	let query: Result<Ticket, _> = sqlx::query_as("select * from tickets where id=$1")
		.bind(ticket_id)
		.fetch_one(&pool)
		.await;

	if let Err(e) = query {
		admin_logger(LogType::Error, &format!("Error reading ticket from db: {}", e), None)
			.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}
	let mut ticket = query.unwrap();

	if ticket.status == "closed" {
		admin_logger(LogType::Error, 
			&format!("Attempt to update closed ticket. id: {}, user_id: {}", ticket.id, payload.user_id),
			None)
			.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
		return Err(StatusCode::FORBIDDEN);
	}

	// remove the ticket from user_active_tickets
	let query = sqlx::query("update user_active_tickets set active=false where ticketid=$1 and userid=$2")
		.bind(ticket_id)
		.bind(payload.user_id)
		.execute(&mut *tx)
		.await;

	if let Err(e) = query {
		log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket_id, e), ticket.log_id)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}

	// user rejected the ticket
	if !payload.status {
		let query = sqlx::query("update tickets set status='rejected' where id=$1")
			.bind(ticket_id)
			.execute(&mut *tx)
			.await;
		if let Err(e) = query {
			log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket_id, e), ticket.log_id)?;
			return Err(StatusCode::INTERNAL_SERVER_ERROR);
		}

		let query = sqlx::query("update user_active_tickets set active=false where ticketid=$1")
			.bind(ticket_id)
			.execute(&mut *tx)
			.await;
		if let Err(e) = query {
			log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket_id, e), ticket.log_id)?;
			return Err(StatusCode::INTERNAL_SERVER_ERROR);
		}
		log(LogType::Rejection, 
			format!("Ticket {} rejected by {}, message: {:?}", ticket.id, payload.user_id, payload.data),
			ticket.log_id)?;
	}
	else {
		// user accepted the ticket
		// update the state
		match payload.data.clone() {
			Some(mut data) => {
				let mut new_state = serde_json::value::from_value::<Map<String, serde_json::Value>>(ticket.state).unwrap();
				new_state.append(&mut data);
				ticket.state = serde_json::Value::Object(new_state);
			}
			None => {}
		}
		// process the update
		let result = update_internal(&mut ticket, &payload).await;
		if let Err(e) = result {
			log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket.id, e), ticket.log_id)?;
			return Err(StatusCode::INTERNAL_SERVER_ERROR);
		}

		for new_ticket in result.unwrap() {
			match new_ticket.type_ {
				NewUserTicketType::ApproveRequest => {
					let new_ticket_username = new_ticket.username.unwrap();
					let userid_query: Result<Userid, _> = sqlx::query_as("select userid from users where username=$1")
						.bind(&new_ticket_username)
						.fetch_one(&mut *tx)
						.await;

					if let Err(e) = userid_query {
						log(LogType::Error, format!("Error reading userid from db: {}", e), ticket.log_id)?;
						return Err(StatusCode::INTERNAL_SERVER_ERROR);
					}
					let userid = userid_query.unwrap();


					let query = sqlx::query("insert into user_active_tickets (userid, ticketid, active, node_number, type_) values ($1, $2, $3, $4, $5)")
						.bind(userid.userid)
						.bind(new_ticket.ticket_id)
						.bind(true)
						.bind(new_ticket.node)
						.bind("approve")
						.execute(&mut *tx)
						.await;
					if let Err(e) = query {
						log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket.id, e), ticket.log_id)?;
						return Err(StatusCode::INTERNAL_SERVER_ERROR);
					}
					log(LogType::Request, format!("Ticket {} approval requested from {}", ticket.id, userid.userid), ticket.log_id)?;
				}
				NewUserTicketType::Notify => {
					// insert a new ticket into notifications table and ping the notifier server

					let owner_name_query: Result<Username, _> = sqlx::query_as("select username from users where userid=$1")
						.bind(ticket.owner_id)
						.fetch_one(&mut *tx)
						.await;

					if let Err(e) = owner_name_query {
						admin_logger(LogType::Error, &format!("failed to get owner name in notification NewUserTicket. create request from {}. Error: {}", ticket.owner_id, e), None)
							.map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;
						return Err(StatusCode::INTERNAL_SERVER_ERROR);
					}
					let message = format!("Ticket created by {}. Process Id: {}", ticket.owner_id, ticket.process_id);

					let notified_username = new_ticket.username.as_ref().unwrap();

					// add notification
					let query = sqlx::query("insert into notifications (userid, message, created_at) values ((select userid from users where username=$1), $2, $3)")
						.bind(notified_username)
						.bind(message)
						.bind(chrono::Utc::now())
						.execute(&mut *tx)
						.await;

					if let Err(e) = query {
						admin_logger(LogType::Error, &format!("failed to add notification in NewUserTicket. create request from {}, Error: {}", ticket.owner_id, e), None)
							.map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;
						return Err(StatusCode::INTERNAL_SERVER_ERROR);
					}
					
					log(LogType::NotificationSuccess, format!("Notification sent to notifier for user {} notified for ticket {}", notified_username, ticket.id), ticket.log_id)
						.map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;
				}
				NewUserTicketType::Completion => {
					// this ticket is always the last in the new_ticket_queue because it requires all other nodes to be executed first
					let query = sqlx::query("update user_active_tickets set active=false where ticketid=$1")
						.bind(ticket_id)
						.execute(&mut *tx)
						.await;
					if let Err(e) = query {
						log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket.id, e), ticket.log_id)?;
						return Err(StatusCode::INTERNAL_SERVER_ERROR);
					}
					ticket.status = "closed".to_string();
					log(LogType::Completion, format!("Ticket {} completed", ticket.id), ticket.log_id)?;
				}	
			}
		}

		// update all fields of the ticket
		// TODO: there may be a better way of doing this, serializing multiple times here i think.
		let final_state = serde_json::value::from_value::<Map<String, serde_json::Value>>(ticket.state).unwrap();
		let query = sqlx::query("update tickets set status=$1, complete=$2, updated_at=$3, state=$4 where id=$5")
			.bind(&ticket.status)
			.bind(ticket.complete)
			.bind(ticket.updated_at)
			.bind(&serde_json::Value::Object(final_state))
			.bind(ticket_id)
			.execute(&mut *tx)
			.await;

		if let Err(e) = query {
			log(LogType::Error, format!("Error updating ticket: id = {} :  {:?}",ticket.id, e), ticket.log_id)?;
			return Err(StatusCode::INTERNAL_SERVER_ERROR);
		}
	}


	if let Err(e) = tx.commit().await {
		log(LogType::Error, format!("Error commiting transaction: {} for pid {}", e, ticket_id), ticket.log_id)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);

	}
	return Ok(StatusCode::ACCEPTED);
}

async fn update_internal(ticket: &mut Ticket, request: &UpdateTicket) -> Result<Vec<NewUserTicket>, ExecuteErr> {
	let mut node_queue = VecDeque::new();
	let mut ticket_queue = Vec::new();
	let process_data = read_process_data(ticket.process_id.clone());
	if let Err(e) = process_data {
		log(LogType::Error, format!("Error reading process data: {}", e), ticket.log_id)
			.map_err(|_| ExecuteErr::FailedToLog)?;
		return Err(ExecuteErr::FailedToReadProcessData);
	}
	let process_data = process_data.unwrap();
	// process the first request
	// TODO: currently exec_user_request will not return any new ticket that has to be added. this may change later
	let result = execute_user_request(ticket, request.node, request.data.as_ref()).await?;
	node_queue.extend(result.completable_steps.iter());

	// FIXME: cleanup this code
	while let Some(node) = node_queue.pop_front() {
		let result = execute_completable(ticket, node, &process_data).await?;
		if !result.completable_steps.is_empty() {
			node_queue.extend(result.completable_steps.iter());
		}

		if let Some(t) = result.new_ticket {
			ticket_queue.push(t);
		}
	}
	return Ok(ticket_queue);
}

async fn execute_user_request(ticket: &mut Ticket, current_node: i32, data: Option<&Map<String, serde_json::Value>>) -> Result<SingleExecState, ExecuteErr>{
	let process_data = read_process_data(ticket.process_id.clone());
	if let Err(e) = process_data {
		log(LogType::Error, format!("Error reading process data: {}", e), ticket.log_id)
			.map_err(|_| ExecuteErr::FailedToLog)?;
		return Err(ExecuteErr::FailedToReadProcessData);
	}

	let process_data = process_data.unwrap();
	let current_job = process_data.steps[current_node as usize].clone();

	// execute the callback for the current node
	// if the current step is a BlockingTask then the callbacks have already been completed,
	// this request comes from callback server
	if current_job.is_not_blocking_task() {
		let current_callbacks = current_job.callbacks.unwrap_or(vec![]);
		if !current_callbacks.is_empty() {
			let ticket_id = ticket.id;
			let data = match data {
				Some(d) => Some(d.clone()),
				None => None
			};
			tokio::spawn(async move {
				send_task(ticket_id, current_node,&data, &current_callbacks).await;
			});
		}
	}
	
	let event = current_job.event;

	let mut result = SingleExecState {
		status: TicketStatus::Open,
		completable_steps: Vec::new(),
		new_ticket: None
	};

	let next_steps = current_job.next;

	match event {
		Event::Initiate => {
			ticket.complete |= 1 << current_node;
			ticket.update_time();
			if next_steps.is_empty() {
				result.status = TicketStatus::Closed;
			}
			log(LogType::Info, format!("Ticket {} initiated succssfully", ticket.id), ticket.log_id)
				.map_err(|_| ExecuteErr::FailedToLog)?;
		}
		Event::Approve => {
			ticket.complete |= 1 << current_node;
			ticket.update_time();
			log(LogType::Approval, 
				format!("Ticket {} approved by {}", ticket.id, current_job.args.unwrap()[0]),
				ticket.log_id)
				.map_err(|_| ExecuteErr::FailedToLog)?;
		}
		Event::Notify => {
			// this event cannot be reached through user request.
			// Notify nodes in a process are completed instantly and only send a notification about the ticket to the given user
			log(LogType::Error, format!("Attempt to complete notify node for tickt {} from {}", ticket.id, current_job.args.unwrap()[0]), ticket.log_id)
				.map_err(|_e| ExecuteErr::FailedToLog)?;
			return Err(ExecuteErr::InvalidTicket);
		}
		Event::Complete => {
			// no user should be able to complete this event
			log(LogType::Error, format!("Attempt to complete ticket {} from {}", ticket.id, current_job.args.unwrap()[0]), ticket.log_id)
				.map_err(|_| ExecuteErr::FailedToLog)?;
			return Err(ExecuteErr::InvalidTicket);
		},
		Event::NonBlockingTask => {
			// Same as Event::Notify. cannot be reached through user request
			log(LogType::Error, format!("Attempt to execute NonBlockingTask node ticket {} from {}", ticket.id, current_job.args.unwrap()[0]), ticket.log_id)
				.map_err(|_| ExecuteErr::FailedToLog)?;
			return Err(ExecuteErr::InvalidTicket);
		},
		Event::BlockingTask => {
			// This node is only reachable from callbacks
			ticket.complete |= 1 << current_node;
			ticket.update_time();
			log(LogType::Approval, 
				format!("Ticket {} approved from callback", ticket.id),
				ticket.log_id)
				.map_err(|_| ExecuteErr::FailedToLog)?;
		}
	}

	for step in next_steps {
		let next_job = process_data.steps[step as usize].clone();
		
		if let Event::Complete = next_job.event {
			if utils::check_n_complete(ticket.complete, process_data.steps.len() as i32) {
				result.completable_steps.push(step);
			}
		}
		else if utils::check_required_complete(ticket.complete, &next_job.required) {
			result.completable_steps.push(step);
		}
	}


	return Ok(result);
}
async fn execute_completable(ticket: &mut Ticket, current_node: i32, process: &Process) -> Result<SingleExecState, ExecuteErr>{
	let mut result = SingleExecState {
		status: TicketStatus::Open,
		completable_steps: Vec::new(),
		new_ticket: None
	};
	let current_job = process.steps[current_node as usize].clone();
	// TODO: callbacks with data for completable steps
	// Approve node reached at this stage is not "completed". we only add a NewUserTicket at this stage so
	// callbacks should only be executed when the node is reached from execute_user_request
	if current_job.is_not_approve() {
		let callbacks = current_job.callbacks.unwrap_or(vec![]);
		if !callbacks.is_empty() {
			let ticket_id = ticket.id;
			tokio::spawn(async move {
				send_task(ticket_id, current_node,&None, &callbacks).await;
			});
		}
	}	
	
	match current_job.event {
		Event::Initiate => {
			// initiate event is only executed when the ticket is first created it wont be executed here again
			return Err(ExecuteErr::InvalidEvent);
		}
		Event::Approve => {
			result.new_ticket = Some(NewUserTicket {
				type_: NewUserTicketType::ApproveRequest,
				ticket_id: ticket.id,
				node: current_node,
				username: Some(current_job.args.unwrap()[0].clone())
			});
		}
		Event::Notify => {
			// this step can be completed right now
			ticket.complete |= 1 << current_node;
			result.new_ticket = Some(NewUserTicket {
				type_: NewUserTicketType::Notify,
				ticket_id: ticket.id,
				node: current_node,
				username: Some(current_job.args.unwrap()[0].clone())
			});
		}
		Event::Complete => {
			ticket.update_time();
			result.new_ticket = Some(NewUserTicket {
				type_: NewUserTicketType::Completion,
				ticket_id: ticket.id,
				node: current_node,
				username: None
			});

			return Ok(result);
		},
		Event::NonBlockingTask => {
			ticket.update_time();
			ticket.complete |= 1 << current_node;
		}
		Event::BlockingTask => {
			// Do nothing. callbacks are already sent so just wait for them to move this node forward
			ticket.update_time();
		},
	}
	let next_steps = current_job.next;
	for step in next_steps {
		let next_job = process.steps[step as usize].clone();
		if let Event::Complete = next_job.event {
			if utils::check_n_complete(ticket.complete, process.steps.len() as i32) {
				result.completable_steps.push(step);
			}
		}
		else if utils::check_required_complete(ticket.complete, &next_job.required) {
			result.completable_steps.push(step);
		}
	}
	return Ok(result);
}

pub async fn get_user_tickets(
	query: extract::Query<GetUserTicketsReq>,
	extract::State(pool): extract::State<sqlx::PgPool>
) -> Result<(StatusCode, Json<UserTickets>), StatusCode> {
	let userid = uuid::Uuid::parse_str(&query.0.userid).unwrap();
	let mut result = UserTickets {
		current_tickets: Vec::new(),
		own_tickets: Vec::new()
	};

	// select all tickets from user_active_tickets of type_!="own"
	let current_ticket_query: Result<Vec<CurrentTicket>, _> = 
		sqlx::query_as(r#"select type_, node_number, ticketid, active, user_active_tickets.userid, process_id, username as owner_name 
			from user_active_tickets join tickets on user_active_tickets.ticketid=tickets.id 
			join users on tickets.owner_id=users.userid
			where user_active_tickets.type_!='own' and user_active_tickets.active='true' and user_active_tickets.userid=$1;"#)
		.bind(userid)
		.fetch_all(&pool)
		.await;
	if let Err(e) = current_ticket_query {
		admin_logger(LogType::Error, &format!("Error reading current tickets: {}", e), None)
			.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}
	result.current_tickets = current_ticket_query.unwrap();

	// select all tickets from tickets where owner_id=userid
	let own_ticket_query: Result<Vec<OwnTicket>, _> = 
		sqlx::query_as("select id, process_id, is_public, created_at, updated_at, status from tickets where owner_id=$1;")
		.bind(userid)
		.fetch_all(&pool)
		.await;

	if let Err(e) = own_ticket_query {
		admin_logger(LogType::Error, &format!("Error reading own tickets: {}", e), None)
			.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
		return Err(StatusCode::INTERNAL_SERVER_ERROR);
	}

	result.own_tickets = own_ticket_query.unwrap();

	return Ok((StatusCode::OK, Json(result)));
}

#[cfg(test)]
mod ticket_tests {
	use crate::db_types::Ticket;
	use dotenv;
use serde_json::Map;

	use super::{update_internal, NewUserTicketType};

	#[tokio::test]
	async fn check_2_node_process() {
		dotenv::dotenv().ok();
		let mut ticket = Ticket {
			id: 0,
			owner_id: uuid::Uuid::new_v4(),
			process_id: "initiate_test".to_string(),
			log_id: uuid::Uuid::new_v4(),
			is_public: false,
			created_at: chrono::Utc::now(),
			updated_at: chrono::Utc::now(),
			status: "open".to_string(),
			complete: 0,
			state: serde_json::Value::Object(Map::new())
		};
		let request = crate::ticket::UpdateTicket {
			ticket_id: 0,
			user_id: uuid::Uuid::new_v4(),
			status: true,
			node: 0,
			data: None
		};

		let result = update_internal(&mut ticket, &request).await;
		assert!(result.is_ok(), "update_internal failed");
		assert_eq!(ticket.complete, 1i32, "ticket complete mask is wrong");
	}

	#[tokio::test]
	async fn check_approve_user_request_works() {
		dotenv::dotenv().ok();
		let mut ticket = Ticket {
			id: 0,
			owner_id: uuid::Uuid::new_v4(),
			process_id: "approve_test".to_string(),
			log_id: uuid::Uuid::new_v4(),
			is_public: false,
			created_at: chrono::Utc::now(),
			updated_at: chrono::Utc::now(),
			status: "open".to_string(),
			// initiate step is already completed
			complete: 1,
			state: serde_json::Value::Object(Map::new())
		};
		let request = crate::ticket::UpdateTicket {
			ticket_id: 0,
			user_id: uuid::Uuid::new_v4(),
			status: true,
			node: 1,
			data: None
		};
		// in this case the user request is completing approve event so the entire process should complete
		let result = update_internal(&mut ticket, &request).await;
		assert!(result.is_ok(), "update_internal failed");
		assert_eq!(ticket.complete, 3i32, "ticket complete mask is wrong");

		let result = result.unwrap();
		assert_eq!(result.len(), 1, "there should be one new ticket in the ticket queue");
		let new_user_ticket = result.get(0).unwrap();
		match new_user_ticket.type_ {
			NewUserTicketType::Completion => {},
			_ => {
				panic!("new ticket should be of type completion");
			}
		}
	}
	#[tokio::test]
	async fn check_approve_node_works() {
		dotenv::dotenv().ok();
		let mut ticket = Ticket {
			id: 0,
			owner_id: uuid::Uuid::new_v4(),
			process_id: "approve_test".to_string(),
			log_id: uuid::Uuid::new_v4(),
			is_public: false,
			created_at: chrono::Utc::now(),
			updated_at: chrono::Utc::now(),
			status: "open".to_string(),
			complete: 0,
			state: serde_json::Value::Object(Map::new())
		};
		let request = crate::ticket::UpdateTicket {
			ticket_id: 0,
			user_id: uuid::Uuid::new_v4(),
			status: true,
			node: 0,
			data: None
		};
		// in this case the user request is completing approve event so the entire process should complete
		let result = update_internal(&mut ticket, &request).await;
		assert!(result.is_ok(), "update_internal failed");
		assert_eq!(ticket.complete, 1i32, "ticket complete mask is wrong");

		let result = result.unwrap();
		assert_eq!(result.len(), 1, "there should be one new ticket in the ticket queue");
		let new_user_ticket = result.get(0).unwrap();
		match new_user_ticket.type_ {
			NewUserTicketType::ApproveRequest => {},
			_ => {
				panic!("new ticket should be of type approve_request");
			}
		}
	}

	#[tokio::test]
	async fn check_branch_process_initiate_works(){
		dotenv::dotenv().ok();
		let mut ticket = Ticket {
			id: 0,
			owner_id: uuid::Uuid::new_v4(),
			process_id: "simple_branch_test".to_string(),
			log_id: uuid::Uuid::new_v4(),
			is_public: false,
			created_at: chrono::Utc::now(),
			updated_at: chrono::Utc::now(),
			status: "open".to_string(),
			complete: 0,
			state: serde_json::Value::Object(Map::new())
		};
		let request = crate::ticket::UpdateTicket {
			ticket_id: 0,
			user_id: uuid::Uuid::new_v4(),
			status: true,
			node: 0,
			data: None
		};

		let result = update_internal(&mut ticket, &request).await;
		assert!(result.is_ok(), "update_internal failed");
		let result = result.unwrap();
		assert_eq!(ticket.complete, 1i32, "ticket complete mask is wrong");

		assert_eq!(result.len(), 2, "only 2 tickets should be added");
		for ticket in result {
			match ticket.type_ {
				NewUserTicketType::ApproveRequest => {},
				_ => {
					panic!("both tickets should be of type NewUSerTicketType::ApproveRequest");
				}
			}
			assert!(ticket.username.is_some(), "ticket should have a username");
			assert_eq!(ticket.username, Some("erp_admin".to_string()), "wrong username added for the approve reequest");
		}
	}
	#[tokio::test]
	async fn check_branch_process_1_approve_works(){
		dotenv::dotenv().ok();
		let mut ticket = Ticket {
			id: 0,
			owner_id: uuid::Uuid::new_v4(),
			process_id: "simple_branch_test".to_string(),
			log_id: uuid::Uuid::new_v4(),
			is_public: false,
			created_at: chrono::Utc::now(),
			updated_at: chrono::Utc::now(),
			status: "open".to_string(),
			complete: 3i32,
			state: serde_json::Value::Object(Map::new())
		};
		let request = crate::ticket::UpdateTicket {
			ticket_id: 0,
			user_id: uuid::Uuid::new_v4(),
			status: true,
			node: 2,
			data: None
		};

		let result = update_internal(&mut ticket, &request).await;
		assert!(result.is_ok(), "update_internal failed");
		let result = result.unwrap();
		// assert_eq!(ticket.complete, , "ticket complete mask is wrong");

		assert_eq!(result.len(), 1, "only 1 tickets should be added");
		let t = result.get(0).unwrap();
		match t.type_ {
			NewUserTicketType::ApproveRequest => {},
			_ => {
				panic!("both tickets should be of type NewUSerTicketType::ApproveRequest");
			}
		}
		assert!(t.username.is_some(), "ticket should have a username");
		assert_eq!(t.username, Some("erp_admin".to_string()), "wrong username added for the approve request");
	}
}