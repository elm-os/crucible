# Alerts and egress

Alert rules notify channels: webhooks, Slack and email. This page covers what the server needs for them and how to
limit the calls it makes. Creating rules and channels, and verifying a webhook's signature, are in the
[alerts guide](../guides/alerts.md). Alerts need the control plane (Postgres).

## How notifications leave the server

- Rules are evaluated on schedule by the servers of the deployment; each evaluation is claimed by one server.
- A notification is written to an outbox in Postgres in the same transaction as the state change that caused it,
  so a restart loses none. Each server delivers from the outbox.
- A failed delivery is retried after 5 seconds, doubling up to 1 hour. After 50 failed deliveries in a row the
  channel is turned off, with the reason on the channel; turning it back on clears the count.
- Webhook and Slack deliveries are HTTP `POST`s with a 5-second connect timeout and a 10-second response timeout.
  They never follow a redirect.
- Each delivery resolves the target's host, checks every address it resolves to against the egress policy below,
  and connects to the address it checked.

## The egress policy

| `egress.mode` | Allowed targets |
|---|---|
| `open` (default) | Any `http` or `https` address, private networks and plain `http` included, except cloud metadata addresses |
| `strict` | Public `https` addresses only. Loopback, private, link-local, carrier-grade NAT and reserved ranges are refused, and so are the hosts of the server's own ClickHouse and Postgres. `egress.allow_cidrs` lets named ranges through |

Cloud metadata addresses are refused in both modes, and no setting allows them: `169.254.169.0/24`,
`169.254.170.0/24`, `100.100.100.200`, `168.63.129.16`, `192.0.0.192`, `fd00:ec2::/32`, `fe80::a9fe:a9fe` and
`fd20:ce::254`, in every spelling, including IPv4 addresses written inside IPv6 ones.

The policy is checked when a channel is saved and again at every delivery and test. A refused address answers
`ssrf_blocked` with what to change:

```text
10.20.1.5 is a private network address and egress.mode = strict refuses it; add its range to egress.allow_cidrs
(CRUCIBLE_EGRESS_ALLOW_CIDRS), or set egress.mode = open
```

To run strict but reach an internal receiver:

```yaml
      CRUCIBLE_EGRESS_MODE: strict
      CRUCIBLE_EGRESS_ALLOW_CIDRS: 10.20.0.0/16
```

## Email

Email channels send through the mail server named by `smtp.*`; without `smtp.host` and `smtp.from` they cannot be
created, and the refusal names both settings. Restart the server after setting them.

| Setting | Default | Meaning |
|---|---|---|
| `smtp.host`, `smtp.from` | none | The mail server and the sender address |
| `smtp.port` | 587; 465 with `tls`; 25 with `none` | |
| `smtp.tls` | `starttls` | `starttls`, `tls`, or `none` for a relay on a network you trust |
| `smtp.username`, `smtp.password` or `smtp.password_file` | none | Credentials, when the mail server asks for them |

For example, against a relay on the internal network:

```yaml
      CRUCIBLE_SMTP_HOST: mail.internal
      CRUCIBLE_SMTP_PORT: "25"
      CRUCIBLE_SMTP_TLS: none
      CRUCIBLE_SMTP_FROM: crucible@example.com
```

The egress policy covers webhook and Slack deliveries; email goes to the configured mail server. A channel's
**Send test** delivers one message and reports whether the receiver accepted it.
