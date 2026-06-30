#[cfg(test)]
mod tests {
    use crate::translation::ensure_translations;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_translate() {
        let mut tr = HashMap::new();
        tr.insert("en".to_string(), "Pacha Mama".to_string());
        tr.insert("ar".to_string(), "".to_string());

        unsafe { std::env::set_var("GOOGLE_TRANSLATE_API_KEY", "your_api_key_here") }; //("GOOGLE_TRANSLATE_API_KEY", "your_api_key_here");

        let res = ensure_translations(&mut tr).await;
        println!("Result: {:?}", res);
        println!("Map: {:?}", tr);
        assert_eq!(tr.get("ar").unwrap(), "باشا ماما");
    }
}
