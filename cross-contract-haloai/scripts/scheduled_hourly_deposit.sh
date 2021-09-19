# Deposits one NEAR into the account every 1 hour

near call cron.$NEAR_ACCT create_task '{"contract_id": "counter.'$NEAR_ACCT'","function_id": "increment","cadence": "0 * * * *","recurring": true,"deposit": "0","gas": 2400000000000}' --accountId counter.$NEAR_ACCT --amount 1