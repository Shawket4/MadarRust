-- Push notifications (APP-6): the live phone's FCM token and the language its
-- pushes are written in.
ALTER TABLE staff_devices
    ADD COLUMN push_token  text,
    ADD COLUMN push_locale text NOT NULL DEFAULT 'ar';
