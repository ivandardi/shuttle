use std::{fmt::Formatter, str::FromStr};

use async_trait::async_trait;
use axum::{
    extract::{FromRef, FromRequestParts},
    headers::{authorization::Bearer, Authorization},
    http::request::Parts,
    TypedHeader,
};
use serde::{Deserialize, Deserializer, Serialize};
use shuttle_common::{
    claims::{Scope, ScopeBuilder},
    ApiKey,
};
use sqlx::{query, Row, SqlitePool};
use tracing::{debug, trace, Span};

use crate::{api::UserManagerState, error::Error};

#[async_trait]
pub trait UserManagement: Send + Sync {
    async fn create_user(&self, name: AccountName, tier: AccountTier) -> Result<User, Error>;
    async fn get_user(&self, name: AccountName) -> Result<User, Error>;
    async fn get_user_by_key(&self, key: ApiKey) -> Result<User, Error>;
    async fn reset_key(&self, name: AccountName) -> Result<(), Error>;
}

#[derive(Clone)]
pub struct UserManager {
    pub pool: SqlitePool,
}

#[async_trait]
impl UserManagement for UserManager {
    async fn create_user(&self, name: AccountName, tier: AccountTier) -> Result<User, Error> {
        let key = ApiKey::generate();

        query("INSERT INTO users (account_name, key, account_tier) VALUES (?1, ?2, ?3)")
            .bind(&name)
            .bind(&key)
            .bind(tier)
            .execute(&self.pool)
            .await?;

        Ok(User::new(name, key, tier))
    }

    async fn get_user(&self, name: AccountName) -> Result<User, Error> {
        query("SELECT account_name, key, account_tier FROM users WHERE account_name = ?1")
            .bind(&name)
            .fetch_optional(&self.pool)
            .await?
            .map(|row| User {
                name,
                key: row.try_get("key").unwrap(),
                account_tier: row.try_get("account_tier").unwrap(),
            })
            .ok_or(Error::UserNotFound)
    }

    async fn get_user_by_key(&self, key: ApiKey) -> Result<User, Error> {
        query("SELECT account_name, key, account_tier FROM users WHERE key = ?1")
            .bind(&key)
            .fetch_optional(&self.pool)
            .await?
            .map(|row| User {
                name: row.try_get("account_name").unwrap(),
                key,
                account_tier: row.try_get("account_tier").unwrap(),
            })
            .ok_or(Error::UserNotFound)
    }

    async fn reset_key(&self, name: AccountName) -> Result<(), Error> {
        let key = ApiKey::generate();

        let rows_affected = query("UPDATE users SET key = ?1 WHERE account_name = ?2")
            .bind(&key)
            .bind(&name)
            .execute(&self.pool)
            .await?
            .rows_affected();

        if rows_affected > 0 {
            Ok(())
        } else {
            Err(Error::UserNotFound)
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize, Debug)]
pub struct User {
    pub name: AccountName,
    pub key: ApiKey,
    pub account_tier: AccountTier,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.account_tier == AccountTier::Admin
    }

    pub fn new(name: AccountName, key: ApiKey, account_tier: AccountTier) -> Self {
        Self {
            name,
            key,
            account_tier,
        }
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for User
where
    S: Send + Sync,
    UserManagerState: FromRef<S>,
{
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let key = Key::from_request_parts(parts, state).await?;

        let user_manager: UserManagerState = UserManagerState::from_ref(state);

        let user = user_manager
            .get_user_by_key(key.into())
            .await
            // Absorb any error into `Unauthorized`
            .map_err(|_| Error::Unauthorized)?;

        // Record current account name for tracing purposes
        Span::current().record("account.name", &user.name.to_string());

        Ok(user)
    }
}

impl From<User> for shuttle_common::models::user::Response {
    fn from(user: User) -> Self {
        Self {
            name: user.name.to_string(),
            key: user.key.as_ref().to_string(),
            account_tier: user.account_tier.to_string(),
        }
    }
}

/// A wrapper around [ApiKey] so we can implement [FromRequestParts] for it.
pub struct Key(ApiKey);

impl From<Key> for ApiKey {
    fn from(key: Key) -> Self {
        key.0
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for Key
where
    S: Send + Sync,
{
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let key = TypedHeader::<Authorization<Bearer>>::from_request_parts(parts, state)
            .await
            .map_err(|_| Error::KeyMissing)
            .and_then(|TypedHeader(Authorization(bearer))| {
                let bearer = bearer.token().trim();
                ApiKey::parse(bearer).map_err(|error| {
                    debug!(error = ?error, "received a malformed api-key");
                    Self::Rejection::Unauthorized
                })
            })?;

        trace!("got bearer key");

        Ok(Key(key))
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize, Debug, sqlx::Type, strum::Display)]
#[sqlx(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[derive(Default)]
pub enum AccountTier {
    #[default]
    Basic,
    Pro,
    Team,
    Admin,
    Deployer,
}

impl From<AccountTier> for Vec<Scope> {
    fn from(tier: AccountTier) -> Self {
        let mut builder = ScopeBuilder::new();

        if tier == AccountTier::Admin {
            builder = builder.with_admin()
        }

        if tier == AccountTier::Deployer {
            builder = builder.with_deploy_rights();
        } else {
            builder = builder.with_basic();
        }

        builder.build()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type, Serialize)]
#[sqlx(transparent)]
pub struct AccountName(String);

impl From<String> for AccountName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl FromStr for AccountName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(s.to_string().into())
    }
}

impl std::fmt::Display for AccountName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for AccountName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

pub struct Admin {
    pub user: User,
}

#[async_trait]
impl<S> FromRequestParts<S> for Admin
where
    S: Send + Sync,
    UserManagerState: FromRef<S>,
{
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = User::from_request_parts(parts, state).await?;

        if user.is_admin() {
            Ok(Self { user })
        } else {
            Err(Error::Forbidden)
        }
    }
}

#[cfg(test)]
mod tests {
    mod convert_tiers {
        use shuttle_common::claims::Scope;

        use crate::user::AccountTier;

        #[test]
        fn basic() {
            let scopes: Vec<Scope> = AccountTier::Basic.into();

            assert_eq!(
                scopes,
                vec![
                    Scope::Deployment,
                    Scope::DeploymentPush,
                    Scope::Logs,
                    Scope::Service,
                    Scope::ServiceCreate,
                    Scope::Project,
                    Scope::ProjectCreate,
                    Scope::Resources,
                    Scope::ResourcesWrite,
                    Scope::Secret,
                    Scope::SecretWrite,
                ]
            );
        }

        #[test]
        fn pro() {
            let scopes: Vec<Scope> = AccountTier::Pro.into();

            assert_eq!(
                scopes,
                vec![
                    Scope::Deployment,
                    Scope::DeploymentPush,
                    Scope::Logs,
                    Scope::Service,
                    Scope::ServiceCreate,
                    Scope::Project,
                    Scope::ProjectCreate,
                    Scope::Resources,
                    Scope::ResourcesWrite,
                    Scope::Secret,
                    Scope::SecretWrite,
                ]
            );
        }

        #[test]
        fn team() {
            let scopes: Vec<Scope> = AccountTier::Team.into();

            assert_eq!(
                scopes,
                vec![
                    Scope::Deployment,
                    Scope::DeploymentPush,
                    Scope::Logs,
                    Scope::Service,
                    Scope::ServiceCreate,
                    Scope::Project,
                    Scope::ProjectCreate,
                    Scope::Resources,
                    Scope::ResourcesWrite,
                    Scope::Secret,
                    Scope::SecretWrite,
                ]
            );
        }

        #[test]
        fn admin() {
            let scopes: Vec<Scope> = AccountTier::Admin.into();

            assert_eq!(
                scopes,
                vec![
                    Scope::User,
                    Scope::UserCreate,
                    Scope::AcmeCreate,
                    Scope::CustomDomainCreate,
                    Scope::CustomDomainCertificateRenew,
                    Scope::GatewayCertificateRenew,
                    Scope::Admin,
                    Scope::Deployment,
                    Scope::DeploymentPush,
                    Scope::Logs,
                    Scope::Service,
                    Scope::ServiceCreate,
                    Scope::Project,
                    Scope::ProjectCreate,
                    Scope::Resources,
                    Scope::ResourcesWrite,
                    Scope::Secret,
                    Scope::SecretWrite,
                ]
            );
        }

        #[test]
        fn deployer_machine() {
            let scopes: Vec<Scope> = AccountTier::Deployer.into();

            assert_eq!(
                scopes,
                vec![
                    Scope::DeploymentPush,
                    Scope::Resources,
                    Scope::Service,
                    Scope::ResourcesWrite
                ]
            );
        }
    }
}
