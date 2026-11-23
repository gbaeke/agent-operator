---
name: sla-credit
description: Work out the SLA service credit a customer is owed after an outage.
---

# SLA credit

1. Find the outage in the incident postmortems: its start, end and which customers it hit.
2. Find the customer's plan (Starter, Pro or Enterprise), fleet size and monthly QFC fee per unit, and the credit table, in pricing-and-sla.md.
3. Monthly uptime = (minutes in the month − outage minutes) / minutes in the month. Use calculate. pricing-and-sla.md counts every month as 30 days (43,200 minutes).
4. Look up the credit percentage for that uptime in the column of the customer's plan.
5. Credit = percentage × units × monthly QFC fee per unit. Use calculate.
6. Answer with the uptime (two decimals), the percentage, the amount in euros and each source document.

Use `worked-example.md` (call load_skill with file) to check your steps.
