use std::{collections::HashSet, fs::File};

use rand::{
    seq::{IteratorRandom, SliceRandom},
    thread_rng,
};
use serde::{Deserialize, Serialize};

use crate::utils::files::read_file_lines;

use super::{
    account::Account,
    constants::{DB_FILE_PATH, PRIVATE_KEYS_FILE_PATH, PROXIES_FILE_PATH, RECIPIENTS_FILE_PATH},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Database(pub Vec<Account>);

impl Database {
    async fn read_from_file(file_path: &str) -> eyre::Result<Self> {
        let contents = tokio::fs::read_to_string(file_path).await?;
        let db = serde_json::from_str::<Self>(&contents)?;
        Ok(db)
    }
    
    /// 保存数据库到文件
    pub async fn save(&self) -> eyre::Result<()> {
        let db_file = File::create(DB_FILE_PATH)?;
        serde_json::to_writer_pretty(db_file, &self.0)?;
        println!("Database saved to {}", DB_FILE_PATH);
        Ok(())
    }
    
    /// 与私钥文件同步，添加新私钥，移除不存在的私钥
    pub async fn sync_with_private_keys(&mut self, passphrase: &str) -> eyre::Result<()> {
        println!("Syncing database with private keys file...");
        
        let mut private_keys = Vec::new();
        
        // 读取当前的私钥文件
        let age_file_path = format!("{}.age", PRIVATE_KEYS_FILE_PATH);
        if std::path::Path::new(&age_file_path).exists() {
            match tokio::fs::read(&age_file_path).await {
                Ok(encrypted_content) => {
                    match crate::utils::crypto::decrypt(std::io::Cursor::new(&encrypted_content), passphrase) {
                        Ok(decrypted_content) => {
                            private_keys = decrypted_content
                                .lines()
                                .map(|line| line.trim().to_string())
                                .filter(|line| !line.is_empty())
                                .collect();
                        }
                        Err(e) => {
                            return Err(eyre::eyre!("Failed to decrypt private keys: {}", e));
                        }
                    }
                }
                Err(e) => {
                    return Err(eyre::eyre!("Failed to read encrypted private keys: {}", e));
                }
            }
        }
        
        if private_keys.is_empty() {
            println!("No private keys found, keeping existing database");
            return Ok(());
        }
        
        let proxies = read_file_lines(PROXIES_FILE_PATH).await.unwrap_or_default();
        let recipients = read_file_lines(RECIPIENTS_FILE_PATH).await.unwrap_or_default();
        
        // 获取数据库中现有的私钥
        let existing_private_keys: HashSet<String> = self.0
            .iter()
            .map(|acc| acc.get_private_key().to_string())
            .collect();
        
        // 添加新的私钥（在文件中存在但在数据库中不存在的）
        let mut added_count = 0;
        for (i, private_key) in private_keys.iter().enumerate() {
            if !existing_private_keys.contains(private_key) {
                let proxy = proxies.get(i).cloned();
                let recipient = recipients.get(i).cloned();
                let account = Account::new(private_key, proxy, recipient);
                self.0.push(account);
                added_count += 1;
            }
        }
        
        // 移除不存在的私钥（在数据库中存在但在文件中不存在的）
        let private_keys_set: HashSet<String> = private_keys.into_iter().collect();
        let mut removed_count = 0;
        self.0.retain(|acc| {
            if private_keys_set.contains(acc.get_private_key()) {
                true
            } else {
                removed_count += 1;
                false
            }
        });
        
        // 为没有用户名的现有账户生成用户名
        let mut username_assigned_count = 0;
        for account in &mut self.0 {
            if account.get_username().is_none() {
                let username = crate::utils::misc::generate_random_username();
                account.set_username(&username);
                username_assigned_count += 1;
            }
        }
        
        let should_save = added_count > 0 || removed_count > 0 || username_assigned_count > 0;
        
        if should_save {
            let mut message = format!("Database sync completed: +{} accounts, -{} accounts", added_count, removed_count);
            if username_assigned_count > 0 {
                message.push_str(&format!(", assigned {} usernames", username_assigned_count));
            }
            println!("{}", message);
            self.save().await?;
        } else {
            println!("Database already in sync with private keys file");
        }
        
        Ok(())
    }

    #[allow(unused)]
    pub async fn read() -> eyre::Result<Self> {
        // 优先从现有数据库文件加载，然后与私钥文件同步
        println!("Reading database...");
        
        // 首先尝试从db.json加载现有数据库
        if std::path::Path::new(DB_FILE_PATH).exists() {
            match Self::read_from_file(DB_FILE_PATH).await {
                Ok(mut db) => {
                    println!("Loaded existing database with {} accounts", db.0.len());
                    // 与私钥文件同步
                    let passphrase = crate::utils::crypto::get_passphrase()?;
                    match db.sync_with_private_keys(&passphrase).await {
                        Ok(()) => return Ok(db),
                        Err(e) => {
                            println!("Warning: Failed to sync with private keys: {}", e);
                            println!("Falling back to creating new database from private keys...");
                        }
                    }
                }
                Err(e) => {
                    println!("Warning: Failed to load existing database: {}", e);
                    println!("Creating new database from private keys...");
                }
            }
        } else {
            println!("No existing database found. Creating new database from private keys...");
        }
        
        // 如果上述都失败，从私钥文件创建新数据库
        let passphrase = crate::utils::crypto::get_passphrase()?;
        Self::new(&passphrase).await
    }

    pub async fn new(passphrase: &str) -> eyre::Result<Self> {
        let mut private_keys = Vec::new();
        
        // 尝试读取加密的私钥文件
        let age_file_path = format!("{}.age", PRIVATE_KEYS_FILE_PATH);
        if std::path::Path::new(&age_file_path).exists() {
            match tokio::fs::read(&age_file_path).await {
                Ok(encrypted_content) => {
                    match crate::utils::crypto::decrypt(std::io::Cursor::new(&encrypted_content), passphrase) {
                        Ok(decrypted_content) => {
                            println!("Decrypted content: {:?}", decrypted_content);
                            private_keys = decrypted_content
                                .lines()
                                .map(|line| line.trim().to_string())
                                .filter(|line| !line.is_empty())
                                .collect();
                        }
                        Err(e) => {
                            println!("Warning: Failed to decrypt private keys: {}", e);
                        }
                    }
                }
                Err(e) => {
                    println!("Warning: Failed to read encrypted private keys: {}", e);
                }
            }
        } else {
            println!("Info: No encrypted private keys file found. System will start with empty database.");
        }
        
        let proxies = read_file_lines(PROXIES_FILE_PATH).await.unwrap_or_default();
        let recipients = read_file_lines(RECIPIENTS_FILE_PATH).await.unwrap_or_default();
        let mut data = Vec::with_capacity(private_keys.len());

        // 只为有效的私钥创建账户
        for (i, private_key) in private_keys.iter().enumerate() {
            let proxy = proxies.get(i).cloned();
            let recipient = recipients.get(i).cloned();

            let account = Account::new(private_key, proxy, recipient);
            data.push(account);
        }

        // 保存数据库状态
        let db_file = File::create(DB_FILE_PATH)?;
        serde_json::to_writer_pretty(db_file, &data)?;

        println!("Database initialized with {} accounts", data.len());
        Ok(Self(data))
    }

    pub fn get_random_account_with_filter<F>(&mut self, filter: F) -> Option<&mut Account>
    where
        F: Fn(&Account) -> bool,
    {
        let mut rng = thread_rng();

        self.0
            .iter_mut()
            .filter(|account| filter(account))
            .choose(&mut rng)
    }

    pub fn update(&self) {
        let file = File::create(DB_FILE_PATH).expect("Default database must be vaild");
        let _ = serde_json::to_writer_pretty(file, &self);
    }

    pub fn shuffle(&mut self) {
        self.0.shuffle(&mut thread_rng());
        self.update();
    }

    pub fn get_account_by_address(&self, address: &str) -> Option<&Account> {
        self.0.iter().find(|acc| acc.proxy_address.to_string().eq_ignore_ascii_case(address))
    }

    /// 可变访问：根据地址获取可变引用
    pub fn get_account_by_address_mut(&mut self, address: &str) -> Option<&mut Account> {
        self.0
            .iter_mut()
            .find(|acc| acc.proxy_address.to_string().eq_ignore_ascii_case(address))
    }

    /// 重新加载私钥并更新数据库
    pub async fn reload_private_keys(&mut self, passphrase: &str) -> eyre::Result<()> {
        let mut private_keys = Vec::new();
        
        // 尝试读取加密的私钥文件
        let age_file_path = format!("{}.age", PRIVATE_KEYS_FILE_PATH);
        if std::path::Path::new(&age_file_path).exists() {
            match tokio::fs::read(&age_file_path).await {
                Ok(encrypted_content) => {
                    match crate::utils::crypto::decrypt(std::io::Cursor::new(&encrypted_content), passphrase) {
                        Ok(decrypted_content) => {
                            println!("Reloading private keys, decrypted content: {:?}", decrypted_content);
                            private_keys = decrypted_content
                                .lines()
                                .map(|line| line.trim().to_string())
                                .filter(|line| !line.is_empty())
                                .collect();
                        }
                        Err(e) => {
                            return Err(eyre::eyre!("Failed to decrypt private keys: {}", e));
                        }
                    }
                }
                Err(e) => {
                    return Err(eyre::eyre!("Failed to read encrypted private keys: {}", e));
                }
            }
        } else {
            return Err(eyre::eyre!("No encrypted private keys file found"));
        }
        
        let proxies = read_file_lines(PROXIES_FILE_PATH).await.unwrap_or_default();
        let recipients = read_file_lines(RECIPIENTS_FILE_PATH).await.unwrap_or_default();
        let mut new_data = Vec::with_capacity(private_keys.len());

        // 只为有效的私钥创建账户
        for (i, private_key) in private_keys.iter().enumerate() {
            let proxy = proxies.get(i).cloned();
            let recipient = recipients.get(i).cloned();

            let account = Account::new(private_key, proxy, recipient);
            new_data.push(account);
        }

        // 更新数据库
        self.0 = new_data;
        
        // 保存数据库状态
        let db_file = File::create(DB_FILE_PATH)?;
        serde_json::to_writer_pretty(db_file, &self.0)?;

        println!("Database reloaded with {} accounts", self.0.len());
        Ok(())
    }
}
