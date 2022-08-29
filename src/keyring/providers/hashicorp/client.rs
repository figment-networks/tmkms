use abscissa_core::prelude::*;
use std::collections::{BTreeMap, HashMap};

use super::error::Error;
use hashicorp_vault::{
    client::{EndpointResponse, HttpVerb, TokenData, VaultResponse},
    Client,
};

use serde::{Deserialize, Serialize};

const VAULT_BACKEND_NAME: &str = "transit";
const PUBLIC_KEY_SIZE: usize = 32;
const SIGNATURE_SIZE: usize = 64;
pub const CONSENUS_KEY_TYPE: &str = "ed25519";

pub(crate) struct TendermintValidatorApp {
    client: Client<TokenData>,
    key_name: String,
    public_key_value: Option<[u8; PUBLIC_KEY_SIZE]>,
}

// TODO(tarcieri): check this is actually sound?!
#[allow(unsafe_code)]
unsafe impl Send for TendermintValidatorApp {}

#[derive(Debug, Serialize)]
struct SignRequest {
    input: String, //Base64 encoded
}

#[derive(Debug, Deserialize)]
struct SignResponse {
    signature: String, //Base64 encoded
}

impl TendermintValidatorApp {
    pub fn connect(host: &str, token: &str, key_name: &str) -> Result<Self, Error> {
        //token self lookup
        let mut client = Client::new(host, token)?;
        client.secret_backend(VAULT_BACKEND_NAME);

        let app = TendermintValidatorApp {
            client,
            key_name: key_name.to_owned(),
            public_key_value: None,
        };

        debug!("Initialized with Vault host at {}", host);
        Ok(app)
    }

    //vault read transit/keys/cosmoshub-sign-key
    //GET http://0.0.0.0:8200/v1/transit/keys/cosmoshub-sign-key
    /// Get public key
    pub fn public_key(&mut self) -> Result<[u8; PUBLIC_KEY_SIZE], Error> {
        if let Some(v) = self.public_key_value {
            debug!("using cached public key {}...", self.key_name);
            return Ok(v.clone());
        }

        debug!("fetching public key for {}...", self.key_name);

        ///
        #[derive(Debug, Deserialize)]
        struct PublicKeyResponse {
            keys: BTreeMap<usize, HashMap<String, String>>,
        }

        let data = self.client.call_endpoint::<PublicKeyResponse>(
            HttpVerb::GET,
            &format!("transit/keys/{}", self.key_name),
            None,
            None,
        )?;

        //{ keys: {1: {"name": "ed25519", "public_key": "R5n8OFaknb/3sCTx/aegNzYukwVx0uNtzzK/2RclIOE=", "creation_time": "2022-08-18T12:44:02.136328217Z"}} }
        let data = if let EndpointResponse::VaultResponse(VaultResponse {
            data: Some(data), ..
        }) = data
        {
            data
        } else {
            return Err(Error::InvalidPubKey(
                "Public key: Vault response unavailable".into(),
            ));
        };

        //latest version
        let key_data = data.keys.iter().last();

        let pubk = if let Some((version, map)) = key_data {
            debug!("public key vetion:{}", version);
            if let Some(pubk) = map.get("public_key") {
                if let Some(key_type) = map.get("name") {
                    if CONSENUS_KEY_TYPE != key_type {
                        return Err(Error::InvalidPubKey(format!(
                            "Public key \"{}\": expected key type:{}, received:{}",
                            self.key_name, CONSENUS_KEY_TYPE, key_type
                        )));
                    }
                } else {
                    return Err(Error::InvalidPubKey(format!(
                        "Public key \"{}\": expected key type:{}, unable to determine type",
                        self.key_name, CONSENUS_KEY_TYPE
                    )));
                }
                pubk
            } else {
                return Err(Error::InvalidPubKey(
                    "Public key: unable to retrieve - \"public_key\" key is not found!".into(),
                ));
            }
        } else {
            return Err(Error::InvalidPubKey(
                "Public key: unable to retrieve last version - not available!".into(),
            ));
        };

        debug!("Public key: fetched {}={}...", self.key_name, pubk);

        let pubk = base64::decode(pubk)?;

        debug!(
            "Public key: base64 decoded {}, size:{}",
            self.key_name,
            pubk.len()
        );

        let mut array = [0u8; PUBLIC_KEY_SIZE];
        array.copy_from_slice(&pubk[..PUBLIC_KEY_SIZE]);

        //cache it...
        self.public_key_value = Some(array.clone());
        debug!("Public key: value cached {}", self.key_name,);

        Ok(array)
    }

    //vault write transit/sign/cosmoshub-sign-key plaintext=$(base64 <<< "some-data")
    //"https://127.0.0.1:8200/v1/transit/sign/cosmoshub-sign-key"
    /// Sign message
    pub fn sign(&self, message: &[u8]) -> Result<[u8; SIGNATURE_SIZE], Error> {
        debug!("signing request: received");
        if message.is_empty() {
            return Err(Error::InvalidEmptyMessage);
        }

        let body = SignRequest {
            input: base64::encode(message),
        };

        debug!("signing request: base64 encoded and about to submit for signing...");

        let data = self.client.call_endpoint::<SignResponse>(
            HttpVerb::POST,
            &format!("transit/sign/{}", self.key_name),
            None,
            Some(&serde_json::to_string(&body)?),
        )?;

        debug!("signing request: about to submit for signing...");

        let data = if let EndpointResponse::VaultResponse(VaultResponse {
            data: Some(data), ..
        }) = data
        {
            data
        } else {
            return Err(Error::NoSignature);
        };

        let parts = data.signature.split(":").collect::<Vec<&str>>();
        if parts.len() != 3 {
            return Err(Error::InvalidSignature(format!(
                "expected 3 parts, received:{} full:{}",
                parts.len(),
                data.signature
            )));
        }

        //signature: "vault:v1:/bcnnk4p8Uvidrs1/IX9s66UCOmmfdJudcV1/yek9a2deMiNGsVRSjirz6u+ti2wqUZfG6UukaoSHIDSSRV5Cw=="
        let base64_signature = if let Some(sign) = parts.last() {
            sign.to_owned()
        } else {
            //this should never happen
            return Err(Error::InvalidSignature("last part is not available".into()));
        };

        let signature = base64::decode(base64_signature)?;
        if signature.len() != 64 {
            return Err(Error::InvalidSignature(format!(
                "invalid signature length! 64 == {}",
                signature.len()
            )));
        }

        let mut array = [0u8; SIGNATURE_SIZE];
        array.copy_from_slice(&signature[..SIGNATURE_SIZE]);
        Ok(array)
    }

    //The returned key will be a 4096-bit RSA public key.
    pub fn wrapping_key(&self) -> Result<String, Error> {
        #[derive(Debug, Deserialize)]
        struct PublicKeyResponse {
            public_key: String,
        }

        let data = self.client.call_endpoint::<PublicKeyResponse>(
            HttpVerb::GET,
            "transit/wrapping_key",
            None,
            None,
        )?;

        Ok(
            if let EndpointResponse::VaultResponse(VaultResponse { data: Some(d), .. }) = data {
                debug!("wrapping key:\n{}", d.public_key);
                if let Some(key) = d.public_key.lines().nth(1) {
                    key.to_owned()
                } else {
                    return Err(Error::InvalidPubKey("Error getting wrapping key!".into()));
                }
            } else {
                return Err(Error::InvalidPubKey("Error getting wrapping key!".into()));
            },
        )
    }

    //vault read transit/export/encryption-key/ephemeral-wrapping-key
    pub fn export_key(&self, key_type: &str, key_name: &str) -> Result<String, Error> {
        #[derive(Debug, Deserialize)]
        struct ExportKeyResponse {
            name: String,
            r#type: String,
            keys: BTreeMap<usize, String>,
        }

        let data = self.client.call_endpoint::<ExportKeyResponse>(
            HttpVerb::GET,
            &format!("transit/export/{}/{}", key_type, key_name),
            None,
            None,
        )?;

        Ok(
            if let EndpointResponse::VaultResponse(VaultResponse {
                data: Some(data), ..
            }) = data
            {
                if let Some((_, key)) = data.keys.into_iter().last() {
                    key
                } else {
                    return Err(Error::InvalidPubKey("Error getting wrapping key!".into()));
                }
            } else {
                return Err(Error::InvalidPubKey("Error getting wrapping key!".into()));
            },
        )
    }
}

// pub(super) enum ExportKeyTypeEnum {
//     ENCRYPTION_KEY,
//     SIGNING_KEY,
//     HMAC_KEY,
// }

// impl TryFrom<&str> for ExportKeyTypeEnum {
//     type Error = Error;
//     fn try_from(value: &str) -> Result<Self, Self::Error> {

//     }
// }

#[cfg(feature = "hashicorp")]
#[cfg(test)]
mod tests {
    use super::*;
    use base64;
    use mockito::{mock, server_address};

    const TEST_TOKEN: &str = "test-token";
    const TEST_KEY_NAME: &str = "test-key-name";
    const TEST_PUB_KEY_VALUE: &str = "ng+ab41LawVupIXX3ocMn+AfV2W1DEMCfjAdtrwXND8="; //base64
    const TEST_PAYLOAD_TO_SIGN_BASE64: &str = "cXFxcXFxcXFxcXFxcXFxcXFxcXE="; //$(base64 <<< "qqqqqqqqqqqqqqqqqqqq") => "cXFxcXFxcXFxcXFxcXFxcXFxcXEK", 'K' vs "=" ????
    const TEST_PAYLOAD_TO_SIGN: &[u8] = b"qqqqqqqqqqqqqqqqqqqq";

    const TEST_SIGNATURE:&str = /*vault:v1:*/ "pNcc/FAUu+Ta7itVegaMUMGqXYkzE777y3kOe8AtdRTgLbA8eFnrKbbX/m7zoiC+vArsIUJ1aMCEDRjDK3ZsBg==";

    #[test]
    fn hashicorp_connect_ok() {
        //setup
        let _lookup_self = mock("GET", "/v1/auth/token/lookup-self")
            .match_header("X-Vault-Token", TEST_TOKEN)
            .with_body(TOKEN_DATA)
            .create();

        //test
        let app = TendermintValidatorApp::connect(
            &format!("http://{}", server_address()),
            TEST_TOKEN,
            TEST_KEY_NAME,
        );

        assert!(app.is_ok());
    }

    #[test]
    fn hashicorp_public_key_ok() {
        //setup
        let _lookup_self = mock("GET", "/v1/auth/token/lookup-self")
            .match_header("X-Vault-Token", TEST_TOKEN)
            .with_body(TOKEN_DATA)
            .create();

        //app
        let mut app = TendermintValidatorApp::connect(
            &format!("http://{}", server_address()),
            TEST_TOKEN,
            TEST_KEY_NAME,
        )
        .expect("Failed to connect");

        //Vault call
        let read_key = mock(
            "GET",
            format!("/v1/transit/keys/{}", TEST_KEY_NAME).as_str(),
        )
        .match_header("X-Vault-Token", TEST_TOKEN)
        .with_body(READ_KEY_RESP)
        .expect_at_most(1) //one call only
        .create();

        //server call
        let res = app.public_key();
        assert!(res.is_ok());
        assert_eq!(
            res.unwrap(),
            base64::decode(TEST_PUB_KEY_VALUE).unwrap().as_slice()
        );

        //cached vaule
        let res = app.public_key();
        assert!(res.is_ok());
        assert_eq!(
            res.unwrap(),
            base64::decode(TEST_PUB_KEY_VALUE).unwrap().as_slice()
        );

        read_key.assert();
    }

    #[test]
    fn hashicorp_sign_ok() {
        //setup
        let _lookup_self = mock("GET", "/v1/auth/token/lookup-self")
            .match_header("X-Vault-Token", TEST_TOKEN)
            .with_body(TOKEN_DATA)
            .create();

        //app
        let app = TendermintValidatorApp::connect(
            &format!("http://{}", server_address()),
            TEST_TOKEN,
            TEST_KEY_NAME,
        )
        .expect("Failed to connect");

        let body = serde_json::to_string(&SignRequest {
            input: TEST_PAYLOAD_TO_SIGN_BASE64.into(),
        })
        .unwrap();

        let _sign_mock = mock(
            "POST",
            format!("/v1/transit/sign/{}", TEST_KEY_NAME).as_str(),
        )
        .match_header("X-Vault-Token", TEST_TOKEN)
        .match_body(body.as_str())
        .with_body(SIGN_RESPONSE)
        .create();

        //server call
        let res = app.sign(TEST_PAYLOAD_TO_SIGN);
        assert!(res.is_ok());
        assert_eq!(
            res.unwrap(),
            base64::decode(TEST_SIGNATURE).unwrap().as_slice()
        );
    }

    #[test]
    fn hashicorp_sign_empty_payload_should_fail() {
        //setup
        let _lookup_self = mock("GET", "/v1/auth/token/lookup-self")
            .match_header("X-Vault-Token", TEST_TOKEN)
            .with_body(TOKEN_DATA)
            .create();

        //app
        let app = TendermintValidatorApp::connect(
            &format!("http://{}", server_address()),
            TEST_TOKEN,
            TEST_KEY_NAME,
        )
        .expect("Failed to connect");

        let body = serde_json::to_string(&SignRequest {
            input: TEST_PAYLOAD_TO_SIGN_BASE64.into(),
        })
        .unwrap();

        let _sign_mock = mock(
            "POST",
            format!("/v1/transit/sign/{}", TEST_KEY_NAME).as_str(),
        )
        .match_header("X-Vault-Token", TEST_TOKEN)
        .match_body(body.as_str())
        .with_body(SIGN_RESPONSE)
        .create();

        //server call
        let res = app.sign(&[]);
        assert!(res.is_err());
    }

    //curl --header "X-Vault-Token: hvs.<...valid.token...>>" http://127.0.0.1:8200/v1/auth/token/lookup-self
    const TOKEN_DATA: &str = r#"
    {"request_id":"119fcc9e-85e2-1fcf-c2a2-96cfb20f7446","lease_id":"","renewable":false,"lease_duration":0,"data":{"accessor":"k1g6PqNWVIlKK9NDCWLiTvrG","creation_time":1661247016,"creation_ttl":2764800,"display_name":"token","entity_id":"","expire_time":"2022-09-24T09:30:16.898359776Z","explicit_max_ttl":0,"id":"hvs.CAESIEzWRWLvyYLGlYsCRI_Vt653K26b-cx_lrxBlFo3_2GBGh4KHGh2cy5GVzZ5b25nMVFpSkwzM1B1eHM2Y0ZqbXA","issue_time":"2022-08-23T09:30:16.898363509Z","meta":null,"num_uses":0,"orphan":false,"path":"auth/token/create","policies":["tmkms-transit-sign-policy"],"renewable":false,"ttl":2758823,"type":"service"},"wrap_info":null,"warnings":null,"auth":null}
    "#;

    //curl --header "X-Vault-Token: $VAULT_TOKEN" "${VAULT_ADDR}/v1/transit/keys/<signing_key_name>"
    const READ_KEY_RESP: &str = r#"
    {"request_id":"9cb10d0a-1877-6da5-284b-8ece4b131ae3","lease_id":"","renewable":false,"lease_duration":0,"data":{"allow_plaintext_backup":false,"auto_rotate_period":0,"deletion_allowed":false,"derived":false,"exportable":false,"imported_key":false,"keys":{"1":{"creation_time":"2022-08-23T09:30:16.676998915Z","name":"ed25519","public_key":"ng+ab41LawVupIXX3ocMn+AfV2W1DEMCfjAdtrwXND8="}},"latest_version":1,"min_available_version":0,"min_decryption_version":1,"min_encryption_version":0,"name":"cosmoshub-sign-key","supports_decryption":false,"supports_derivation":true,"supports_encryption":false,"supports_signing":true,"type":"ed25519"},"wrap_info":null,"warnings":null,"auth":null}    
    "#;

    //curl --request POST --header "X-Vault-Token: $VAULT_TOKEN" "${VAULT_ADDR}/v1/transit/sign/<..key_name...>" -d '{"input":"base64 encoded"}'
    const SIGN_RESPONSE: &str = r#"
    {"request_id":"13534911-8e98-9a0f-a701-e9a7736140e2","lease_id":"","renewable":false,"lease_duration":0,"data":{"key_version":1,"signature":"vault:v1:pNcc/FAUu+Ta7itVegaMUMGqXYkzE777y3kOe8AtdRTgLbA8eFnrKbbX/m7zoiC+vArsIUJ1aMCEDRjDK3ZsBg=="},"wrap_info":null,"warnings":null,"auth":null}
    "#;
}