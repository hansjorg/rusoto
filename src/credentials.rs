//! Types for loading and managing AWS access credentials for API requests.

use std::env::*;
use std::env;
use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::io::prelude::*;
use std::io::BufReader;
use std::ascii::AsciiExt;
use std::collections::HashMap;
use hyper::Client;
use hyper::header::Connection;
use error::*;
use regex::Regex;
use chrono::{Duration, UTC, DateTime};
use serde_json::{Value, from_str};
use std::time::Duration as StdDuration;

/// Represents AWS credentials.  Includes access key, secret key, token (for IAM profiles) and expiration timestamp.
#[derive(Clone, Debug)]
pub struct AWSCredentials {
    key: String,
    secret: String,
    token: Option<String>,
    expires_at: DateTime<UTC>
}

impl AWSCredentials {
    /// Create a new `AWSCredentials` from a key ID, secret key, optional access token, and expiry
    /// time.
    pub fn new<K, S>(key:K, secret:S, token:Option<String>, expires_at:DateTime<UTC>)
    -> AWSCredentials where K:Into<String>, S:Into<String> {
        AWSCredentials {
            key: key.into(),
            secret: secret.into(),
            token: token,
            expires_at: expires_at,
        }
    }

    /// Get a reference to the access key ID.
    pub fn aws_access_key_id(&self) -> &str {
    	&self.key
    }

    /// Get a reference to the secret access key.
    pub fn aws_secret_access_key(&self) -> &str {
    	&self.secret
    }

    /// Get a reference to the expiry time.
    pub fn expires_at(&self) -> &DateTime<UTC> {
        &self.expires_at
    }

    /// Get a reference to the access token.
    pub fn token(&self) -> &Option<String> {
        &self.token
    }

    /// Determine whether or not the credentials are expired.
    fn credentials_are_expired(&self) -> bool {
        // This is a rough hack to hopefully avoid someone requesting creds then sitting on them
        // before issuing the request:
        if self.expires_at < UTC::now() + Duration::seconds(20) {
            return true;
        }
        return false;
    }
}

/// A type that produces `AWSCredentials`.
pub trait ProvideAWSCredentials {
    /// Produce a new `AWSCredentials`.
	fn credentials(&mut self) -> Result<&AWSCredentials, AWSError>;
}

fn err(message: &str) -> Result<&AWSCredentials, AWSError> {
    Err(AWSError::new(message))
}

/// Looks for AWS credentials in environment variables.
pub struct EnvironmentCredentialsProvider {
    credentials: Option<AWSCredentials>
}

impl ProvideAWSCredentials for EnvironmentCredentialsProvider {
	fn credentials(&mut self) -> Result<&AWSCredentials, AWSError> {
        if self.credentials.is_none() || self.credentials.as_ref().unwrap().credentials_are_expired() {
           self.credentials = Some(try!(credentials_from_environment()));
        }
        Ok(self.credentials.as_ref().unwrap())
	}
}

impl EnvironmentCredentialsProvider {
    fn new() -> EnvironmentCredentialsProvider {
        EnvironmentCredentialsProvider { credentials: None }
    }

}

fn credentials_from_environment<'a>() -> Result<AWSCredentials, AWSError> {
    let env_key = match var("AWS_ACCESS_KEY_ID") {
        Ok(val) => val,
        Err(_) => return Err(AWSError::new("No AWS_ACCESS_KEY_ID in environment"))
    };
    let env_secret = match var("AWS_SECRET_ACCESS_KEY") {
        Ok(val) => val,
        Err(_) => return Err(AWSError::new("No AWS_SECRET_ACCESS_KEY in environment"))
    };

    if env_key.is_empty() || env_secret.is_empty() {
        return Err(AWSError::new("Couldn't find either AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY or both in environment."));
    }

    Ok(AWSCredentials::new(env_key, env_secret, None, in_ten_minutes()))
}

/// Looks for AWS credentials in a profile in a credentials file.
#[derive(Clone, Debug)]
pub struct ProfileCredentialsProvider {
    credentials: Option<AWSCredentials>,
    file_path: PathBuf,
    profile: String,
}

impl ProfileCredentialsProvider {
    /// Create a new `ProfileCredentialsProvider` for the default credentials file path and profile
    /// name.
    pub fn new() -> AWSResult<ProfileCredentialsProvider> {
        // Default credentials file location:
        // ~/.aws/credentials (Linux/Mac)
        // %USERPROFILE%\.aws\credentials  (Windows)
        let profile_location = match env::home_dir() {
            Some(home_path) => {
                let mut credentials_path = PathBuf::from(".aws");

                credentials_path.push("credentials");

                home_path.join(credentials_path)
            }
            None => return Err(AWSError::new("The environment variable HOME must be set.")),
        };

        Ok(ProfileCredentialsProvider {
            credentials: None,
            file_path: profile_location,
            profile: "default".to_owned(),
        })
    }

    /// Create a new `ProfileCredentialsProvider` for the credentials file at the given path, using
    /// the given profile.
    pub fn with_configuration<F, P>(file_path: F, profile: P) -> ProfileCredentialsProvider
    where F: Into<PathBuf>, P: Into<String> {
        ProfileCredentialsProvider {
            credentials: None,
            file_path: file_path.into(),
            profile: profile.into(),
        }
    }

    /// Get a reference to the credentials file path.
    pub fn file_path(&self) -> &Path {
        self.file_path.as_ref()
    }

    /// Get a reference to the profile name.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Set the credentials file path.
    pub fn set_file_path<F>(&mut self, file_path: F) where F: Into<PathBuf> {
        self.file_path = file_path.into();
    }

    /// Set the profile name.
    pub fn set_profile<P>(&mut self, profile: P) where P: Into<String> {
        self.profile = profile.into();
    }
}

impl ProvideAWSCredentials for ProfileCredentialsProvider {
    fn credentials(&mut self) -> Result<&AWSCredentials, AWSError> {
        if self.credentials.is_none() || self.credentials.as_ref().unwrap().credentials_are_expired() {
            match parse_credentials_file(self.file_path()) {
                Ok(mut profiles) => {
                    let default_profile = profiles.remove(self.profile());
                    if default_profile.is_none() {
                        return err("profile not found");
                    }
                    self.credentials = default_profile;
                },
                Err(_) => { return err("Parse error"); }
            };
       }
       Ok(self.credentials.as_ref().unwrap())
   }
}

fn parse_credentials_file(file_path: &Path) -> Result<HashMap<String, AWSCredentials>, AWSError> {
    match fs::metadata(file_path) {
        Err(_) => return Err(AWSError::new("Couldn't stat credentials file.")),
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(AWSError::new("Couldn't open file."));
            }
        }
    };

    let file = try!(File::open(file_path));

    let profile_regex = Regex::new(r"^\[([^\]]+)\]$").unwrap();
    let mut profiles: HashMap<String, AWSCredentials> = HashMap::new();
    let mut access_key: Option<String> = None;
    let mut secret_key: Option<String> = None;
    let mut profile_name: Option<String> = None;

    let file_lines = BufReader::new(&file);
    for line in file_lines.lines() {

        let unwrapped_line : String = line.unwrap();

        // skip comments
        if unwrapped_line.starts_with('#') {
            continue;
        }

        // handle the opening of named profile blocks
        if profile_regex.is_match(&unwrapped_line) {

            if profile_name.is_some() && access_key.is_some() && secret_key.is_some() {
                let creds = AWSCredentials::new(access_key.unwrap(), secret_key.unwrap(), None, in_ten_minutes());
                profiles.insert(profile_name.unwrap(), creds);
            }

            access_key = None;
            secret_key = None;

            let caps = profile_regex.captures(&unwrapped_line).unwrap();
            profile_name = Some(caps.at(1).unwrap().to_string());
            continue;
        }

        // otherwise look for key=value pairs we care about
        let lower_case_line = unwrapped_line.to_ascii_lowercase().to_string();

        if lower_case_line.contains("aws_access_key_id") {
            if access_key.is_none() {
                let v: Vec<&str> = unwrapped_line.split("=").collect();
                if v.len() > 0 {
                    access_key = Some(v[1].trim_matches(' ').to_string());
                }
            }
        } else if lower_case_line.contains("aws_secret_access_key") {
            if secret_key.is_none() {
                let v: Vec<&str> = unwrapped_line.split("=").collect();
                if v.len() > 0 {
                    secret_key = Some(v[1].trim_matches(' ').to_string());
                }
            }
        }

        // we could potentially explode here to indicate that the file is invalid

    }

    if profile_name.is_some() && access_key.is_some() && secret_key.is_some() {
        let creds = AWSCredentials::new(access_key.unwrap(), secret_key.unwrap(), None, in_ten_minutes());
        profiles.insert(profile_name.unwrap(), creds);
    }

    if profiles.is_empty() {
        return Err(AWSError::new("No credentials found."));
    }

    Ok(profiles)
}

/// Looks for AWS credentials in a resource's IAM role.
pub struct IAMRoleCredentialsProvider {
    credentials: Option<AWSCredentials>
}

impl IAMRoleCredentialsProvider {
    fn new() -> IAMRoleCredentialsProvider {
        IAMRoleCredentialsProvider { credentials: None }
    }
}

impl ProvideAWSCredentials for IAMRoleCredentialsProvider {
    fn credentials(&mut self) -> Result<&AWSCredentials, AWSError> {
        if self.credentials.is_none() || self.credentials.as_ref().unwrap().credentials_are_expired() {
            // TODO: backoff and retry on failure.

            // for "real" use: http://169.254.169.254/latest/meta-data/iam/security-credentials/
            let mut address : String = "http://169.254.169.254/latest/meta-data/iam/security-credentials".to_string();
            let mut client = Client::new();
            client.set_read_timeout(Some(StdDuration::from_secs(15)));
            let mut response;
            match client.get(&address)
                .header(Connection::close()).send() {
                    Err(_) => return err("Couldn't connect to metadata service"), // add Why?
                    Ok(received_response) => response = received_response
                };

            let mut body = String::new();
            match response.read_to_string(&mut body) {
                Err(_) => return err("Didn't get a parsable response body from metadata service"),
                Ok(_) => (),
            };

            address.push_str("/");
            address.push_str(&body);
            body = String::new();
            match client.get(&address)
                .header(Connection::close()).send() {
                    Err(_) => return err("Didn't get a parseable response body from instance role details"),
                    Ok(received_response) => response = received_response
                };

            match response.read_to_string(&mut body) {
                Err(_) => return err("Had issues with reading iam role response: {}"),
                Ok(_) => (),
            };

            let json_object: Value;
            match from_str(&body) {
                Err(_) => return err("Couldn't parse metadata response body."),
                Ok(val) => json_object = val
            };

            let access_key;
            match json_object.find("AccessKeyId") {
                None => return err("Couldn't find AccessKeyId in response."),
                Some(val) => access_key = val.as_string().expect("AccessKeyId value was not a string").to_owned().replace("\"", "")
            };

            let secret_key;
            match json_object.find("SecretAccessKey") {
                None => return err("Couldn't find SecretAccessKey in response."),
                Some(val) => secret_key = val.as_string().expect("SecretAccessKey value was not a string").to_owned().replace("\"", "")
            };

            let expiration;
            match json_object.find("Expiration") {
                None => return err("Couldn't find Expiration in response."),
                Some(val) => expiration = val.as_string().expect("Expiration value was not a string").to_owned().replace("\"", "")
            };

            let expiration_time = try!(expiration.parse());

            let token_from_response;
            match json_object.find("Token") {
                None => return err("Couldn't find Token in response."),
                Some(val) => token_from_response = val.as_string().expect("Token value was not a string").to_owned().replace("\"", "")
            };

            self.credentials = Some(AWSCredentials::new(access_key, secret_key, Some(token_from_response), expiration_time));
        }

		Ok(&self.credentials.as_ref().unwrap())
	}
}

/// Internally chains AWS credential providers in priority order.
///
/// The following sources are checked in order for credentials when calling `credentials`:
///
/// 1. Environment variables: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
/// 2. AWS credentials file. Usually located at `~/.aws/credentials`.
/// 3. IAM instance profile. Will only work if running on an EC2 instance with an instance profile/role.
///
/// If the sources are exhausted without finding credentials, an error is returned.
#[derive(Debug, Clone)]
pub struct DefaultAWSCredentialsProviderChain {
    credentials: Option<AWSCredentials>,
    profile_credentials_provider: ProfileCredentialsProvider,
}

// Chain the providers:
impl ProvideAWSCredentials for DefaultAWSCredentialsProviderChain {
    fn credentials(&mut self) -> Result<&AWSCredentials, AWSError> {
        if self.credentials.is_none() || self.credentials.as_ref().unwrap().credentials_are_expired() {
            // fetch creds in order: env, file, IAM

            if let Ok(creds) = EnvironmentCredentialsProvider::new().credentials() {
                self.credentials = Some(creds.clone());

                return Ok(self.credentials.as_ref().unwrap());
            }

            if let Ok(creds) =  self.profile_credentials_provider.credentials() {
                self.credentials = Some(creds.clone());

                return Ok(self.credentials.as_ref().unwrap());
            }

            if let Ok(creds) = IAMRoleCredentialsProvider::new().credentials() {
                self.credentials = Some(creds.clone());

                return Ok(self.credentials.as_ref().unwrap());
            }

            return Err(AWSError::new("Couldn't find AWS credentials in environment, credentials file, or IAM role."));
        }

        Ok(self.credentials.as_ref().unwrap())
    }
}

impl DefaultAWSCredentialsProviderChain {
    /// Create a new `DefaultAWSCredentialsProviderChain` using a `ProfileCredentialsProvider` with
    /// the default settings.
    pub fn new() -> AWSResult<DefaultAWSCredentialsProviderChain> {
        Ok(DefaultAWSCredentialsProviderChain {
            credentials: None,
            profile_credentials_provider: try!(ProfileCredentialsProvider::new()),
        })
    }

    /// Create a new `DefaultAWSCredentialsProviderChain` using the provided
    /// `ProfileCredentialsProvider`.
    pub fn with_profile_provider(profile_credentials_provider: ProfileCredentialsProvider)
    -> DefaultAWSCredentialsProviderChain {
        DefaultAWSCredentialsProviderChain {
            credentials: None,
            profile_credentials_provider: profile_credentials_provider,
        }
    }
}

fn in_ten_minutes() -> DateTime<UTC> {
    UTC::now() + Duration::seconds(600)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use error::*;

    #[test]
    fn parse_credentials_file_default_profile() {
        let result = super::parse_credentials_file(
            Path::new("tests/sample-data/default_profile_credentials")
        );
        assert!(result.is_ok());

        let profiles = result.ok().unwrap();
        assert_eq!(profiles.len(), 1);

        let default_profile = profiles.get("default").unwrap();
        assert_eq!(default_profile.aws_access_key_id(), "foo");
        assert_eq!(default_profile.aws_secret_access_key(), "bar");
    }

    #[test]
    fn parse_credentials_file_multiple_profiles() {
        let result = super::parse_credentials_file(
            Path::new("tests/sample-data/multiple_profile_credentials")
        );
        assert!(result.is_ok());

        let profiles = result.ok().unwrap();
        assert_eq!(profiles.len(), 2);

        let foo_profile = profiles.get("foo").unwrap();
        assert_eq!(foo_profile.aws_access_key_id(), "foo_access_key");
        assert_eq!(foo_profile.aws_secret_access_key(), "foo_secret_key");

        let bar_profile = profiles.get("bar").unwrap();
        assert_eq!(bar_profile.aws_access_key_id(), "bar_access_key");
        assert_eq!(bar_profile.aws_secret_access_key(), "bar_secret_key");

    }

    #[test]
    fn profile_credentials_provider_happy_path() {
        let mut provider = ProfileCredentialsProvider::with_configuration(
            "tests/sample-data/multiple_profile_credentials",
            "foo",
        );
        let result = provider.credentials();

        assert!(result.is_ok());

        let creds = result.ok().unwrap();
        assert_eq!(creds.aws_access_key_id(), "foo_access_key");
        assert_eq!(creds.aws_secret_access_key(), "foo_secret_key");
     }

    #[test]
    fn profile_credentials_provider_bad_profile() {
        let mut provider = ProfileCredentialsProvider::with_configuration(
            "tests/sample-data/multiple_profile_credentials",
            "not_a_profile",
        );
        let result = provider.credentials();

        assert!(result.is_err());
        assert_eq!(result.err(), Some(AWSError("profile not found".to_string())));
    }

    #[test]
    fn profile_credentials_provider_profile_name() {
       let mut provider = ProfileCredentialsProvider::new().unwrap();
       assert_eq!("default", provider.profile());
       provider.set_profile("foo");
       assert_eq!("foo", provider.profile());
    }

    #[test]
    fn credential_chain_explicit_profile_provider() {
        let profile_provider = ProfileCredentialsProvider::with_configuration(
            "tests/sample-data/multiple_profile_credentials",
            "foo",
        );

        let mut chain = DefaultAWSCredentialsProviderChain::with_profile_provider(profile_provider);

        let credentials = chain.credentials().expect(
            "Failed to get credentials from default provider chain with manual profile",
        );

        assert_eq!(credentials.aws_access_key_id(), "foo_access_key");
        assert_eq!(credentials.aws_secret_access_key(), "foo_secret_key");
    }

    #[test]
    fn existing_file_no_credentials() {
        let result = super::parse_credentials_file(Path::new("tests/sample-data/no_credentials"));
        assert_eq!(result.err(), Some(AWSError::new("No credentials found.")))
    }

    #[test]
    fn parse_credentials_bad_path() {
        let result = super::parse_credentials_file(Path::new("/bad/file/path"));
        assert_eq!(result.err(), Some(AWSError::new("Couldn't stat credentials file.")));
    }

    #[test]
    fn parse_credentials_directory_path() {
        let result = super::parse_credentials_file(Path::new("tests/"));
        assert_eq!(result.err(), Some(AWSError::new("Couldn't open file.")));
    }
}
