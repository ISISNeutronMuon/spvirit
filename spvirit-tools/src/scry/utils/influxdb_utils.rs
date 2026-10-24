use reqwest::Client;

pub struct InfluxDB {
    pub client: Client,
    url: String,
    bucket: String,
    org: String,
    token: String,
}

impl InfluxDB {
    pub fn new(url: String, bucket: String, org: String, token: String) -> Self {
        let client = Client::new();
        InfluxDB {
            client,
            url,
            bucket,
            org,
            token,
        }
    }
    pub async fn ping(&self) -> Result<(), Box<dyn std::error::Error>> {
        let url = format!("{}/health", self.url);
        let res = self.client.get(&url).send().await?;
        match res.error_for_status() {
            Ok(_) => Ok(()),
            Err(e) => Err(Box::new(e)),
        }
    }

    pub async fn check_bucket_exists(&self) -> Result<(), Box<dyn std::error::Error>> {
        let url = format!("{}/api/v2/buckets", self.url);
        let res = self
            .client
            .get(&url)
            .header("Authorization", format!("Token {}", self.token))
            .send()
            .await?;
        match res.error_for_status() {
            Ok(_) => Ok(()),
            Err(e) => Err(Box::new(e)),
        }
    }

    pub async fn write(&self, data: String) -> Result<(), Box<dyn std::error::Error>> {
        let url = format!(
            "{}/api/v2/write?org={}&bucket={}&precision=s",
            self.url, self.org, self.bucket
        );
        let res = self
            .client
            .post(&url)
            .header("Authorization", format!("Token {}", self.token))
            .body(data)
            .send()
            .await?;

        match res.error_for_status() {
            Ok(_) => Ok(()),
            Err(e) => Err(Box::new(e)),
        }
    }
}
