# Security Policy

Agentopsy reads Codex session traces, which can contain prompts, file paths,
commands, tool results, and other sensitive local data. It binds only to
`127.0.0.1:8765` and does not upload trace data or make external requests.

Anyone who can access the local process or browser profile may still see the
rendered trace diagnostics. Exposure caused by granting an untrusted user or
process access to the same machine is not considered an Agentopsy vulnerability.

Please report vulnerabilities in Agentopsy itself privately by emailing
<matthewmckee04@yahoo.co.uk>. Include the affected version, a minimal
reproduction, and the expected impact.

Security fixes target the latest released version and the `main` branch.
