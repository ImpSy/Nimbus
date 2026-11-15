//! Signed-in account: persists the MSA refresh token and lazily derives /
//! caches every downstream token (XBL user, XSTS, GSSV).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::auth::{Auth, MsaToken, Offering, RP_GSSV, RP_XBOXLIVE, StreamToken, XblToken};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    pub msa: MsaToken,
    #[serde(default)]
    pub gamertag: Option<String>,
    #[serde(default)]
    pub xuid: Option<String>,
}

#[derive(Default)]
struct Cache {
    user_token: Option<XblToken>,
    gssv_xsts: Option<XblToken>,
    web_xsts: Option<XblToken>,
    xhome: Option<StreamToken>,
    xcloud: Option<(Offering, StreamToken)>,
    xcloud_unavailable: bool,
}

pub struct Account {
    auth: Auth,
    path: Option<PathBuf>,
    stored: Mutex<StoredAccount>,
    cache: Mutex<Cache>,
}

impl Account {
    /// Load a previously saved account, if any.
    pub fn load(auth: Auth, path: &Path) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        let stored: StoredAccount = serde_json::from_slice(&data).ok()?;
        Some(Self::new(auth, Some(path.to_owned()), stored))
    }

    /// Create from a fresh login and persist it.
    pub fn from_login(auth: Auth, path: Option<&Path>, msa: MsaToken) -> Result<Self> {
        let acc = Self::new(
            auth,
            path.map(Path::to_owned),
            StoredAccount {
                msa,
                gamertag: None,
                xuid: None,
            },
        );
        Ok(acc)
    }

    fn new(auth: Auth, path: Option<PathBuf>, stored: StoredAccount) -> Self {
        Self {
            auth,
            path,
            stored: Mutex::new(stored),
            cache: Mutex::new(Cache::default()),
        }
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    pub async fn stored(&self) -> StoredAccount {
        self.stored.lock().await.clone()
    }

    fn save(&self, stored: &StoredAccount) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(stored)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub async fn persist(&self) -> Result<()> {
        let stored = self.stored.lock().await.clone();
        self.save(&stored)
    }

    pub fn delete_saved(&self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Valid MSA access token, refreshing (and persisting the rotated
    /// refresh token) when needed.
    pub async fn msa_access_token(&self) -> Result<String> {
        let mut stored = self.stored.lock().await;
        if !stored.msa.is_fresh() {
            tracing::info!("refreshing Microsoft account token");
            stored.msa = self.auth.refresh_msa(&stored.msa.refresh_token).await?;
            self.save(&stored)?;
        }
        Ok(stored.msa.access_token.clone())
    }

    async fn user_token(&self) -> Result<XblToken> {
        if let Some(t) = self
            .cache
            .lock()
            .await
            .user_token
            .clone()
            .filter(XblToken::is_fresh)
        {
            return Ok(t);
        }
        let access = self.msa_access_token().await?;
        let t = self.auth.xbl_user_token(&access).await?;
        self.cache.lock().await.user_token = Some(t.clone());
        Ok(t)
    }

    pub async fn gssv_xsts(&self) -> Result<XblToken> {
        if let Some(t) = self
            .cache
            .lock()
            .await
            .gssv_xsts
            .clone()
            .filter(XblToken::is_fresh)
        {
            return Ok(t);
        }
        let user = self.user_token().await?;
        let t = self.auth.xsts_token(&user.token, RP_GSSV).await?;
        self.cache.lock().await.gssv_xsts = Some(t.clone());
        Ok(t)
    }

    /// XSTS token for regular Xbox Live web APIs (profile, achievements...).
    pub async fn web_xsts(&self) -> Result<XblToken> {
        if let Some(t) = self
            .cache
            .lock()
            .await
            .web_xsts
            .clone()
            .filter(XblToken::is_fresh)
        {
            return Ok(t);
        }
        let user = self.user_token().await?;
        let t = self.auth.xsts_token(&user.token, RP_XBOXLIVE).await?;
        {
            let mut stored = self.stored.lock().await;
            if t.gamertag.is_some() && stored.gamertag != t.gamertag {
                stored.gamertag = t.gamertag.clone();
                stored.xuid = t.xuid.clone();
                let _ = self.save(&stored);
            }
        }
        self.cache.lock().await.web_xsts = Some(t.clone());
        Ok(t)
    }

    /// Remote-play (xHome) GSSV token.
    pub async fn xhome_token(&self) -> Result<StreamToken> {
        if let Some(t) = self
            .cache
            .lock()
            .await
            .xhome
            .clone()
            .filter(StreamToken::is_fresh)
        {
            return Ok(t);
        }
        let xsts = self.gssv_xsts().await?;
        let t = self.auth.gssv_login(Offering::XHome, &xsts.token).await?;
        self.cache.lock().await.xhome = Some(t.clone());
        Ok(t)
    }

    /// Cloud gaming GSSV token: Game Pass Ultimate first, falling back to
    /// the free-to-play offering. `None` when xCloud is not available for
    /// this account / country.
    pub async fn xcloud_token(&self) -> Result<Option<(Offering, StreamToken)>> {
        {
            let cache = self.cache.lock().await;
            if let Some((o, t)) = cache.xcloud.as_ref().filter(|(_, t)| t.is_fresh()) {
                return Ok(Some((*o, t.clone())));
            }
            if cache.xcloud_unavailable {
                return Ok(None);
            }
        }
        let xsts = self.gssv_xsts().await?;
        for offering in [Offering::XCloud, Offering::XCloudF2p] {
            match self.auth.gssv_login(offering, &xsts.token).await {
                Ok(t) => {
                    self.cache.lock().await.xcloud = Some((offering, t.clone()));
                    return Ok(Some((offering, t)));
                }
                Err(Error::Http { status, .. }) if (400..500).contains(&status) => {
                    tracing::info!(
                        offering = offering.id(),
                        status,
                        "xCloud offering unavailable"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        self.cache.lock().await.xcloud_unavailable = true;
        Ok(None)
    }

    /// Transfer token for `/connect` on cloud sessions.
    pub async fn transfer_token(&self) -> Result<String> {
        // Make sure the refresh token is still valid first.
        self.msa_access_token().await?;
        let refresh = self.stored.lock().await.msa.refresh_token.clone();
        self.auth.transfer_token(&refresh).await
    }

    /// Drop cached derived tokens (e.g. after a 401).
    pub async fn invalidate(&self) {
        *self.cache.lock().await = Cache::default();
    }
}
