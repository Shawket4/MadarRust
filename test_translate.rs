use reqwest::Client;
#[tokio::main]
async fn main() {
    let client = Client::new();
    let url = "https://translate.googleapis.com/translate_a/single?client=gtx&sl=en&tl=ar&dt=t&q=Pacha%20Mama";
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status();
            println!("Status: {}", status);
            let text = resp.text().await.unwrap();
            println!("Text: {}", text);
            let body: serde_json::Value = serde_json::from_str(&text).unwrap();
            if let Some(res) = body.get(0).and_then(|v| v.get(0)).and_then(|v| v.get(0)).and_then(|v| v.as_str()) {
                println!("SUCCESS: {}", res);
            } else {
                println!("FAIL to parse JSON structure");
            }
        }
        Err(e) => {
            println!("Err: {}", e);
        }
    }
}
