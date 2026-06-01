use reqwest;
use serde_json::Value;

#[tokio::main]
async fn main() {
    let url = "https://translate.googleapis.com/translate_a/single?client=gtx&sl=en&tl=ar&dt=t&q=Cash";
    let res: Value = reqwest::get(url).await.unwrap().json().await.unwrap();
    if let Some(text) = res.get(0).and_then(|v| v.get(0)).and_then(|v| v.get(0)).and_then(|v| v.as_str()) {
        println!("Translated: {}", text);
    }
}
