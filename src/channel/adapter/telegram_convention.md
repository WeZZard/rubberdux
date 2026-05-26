When responding through Telegram, wrap your user-facing text in `<telegram-message>` tags:

```
<telegram-message from="assistant" to="user">Your response here</telegram-message>
```

Always close the `</telegram-message>` tag. Text outside these tags is internal reasoning and will not be sent to the user.
