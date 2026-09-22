Runtime integration:
- Preserve the platform contract: frontend/ has npm run build producing frontend/dist/; backend/ has npm start and reads PORT (default {port}). Within that contract, preserve the existing application architecture and choose libraries or storage appropriate to the requirements and available environment.
- Prefer existing dependencies and avoid unnecessary installation. Do not prohibit frameworks, native modules or durable storage when the task requires them. Use the configured package registry.
- Handle expected request errors with appropriate responses, including 404 for missing resources. Log unexpected failures; do not suppress uncaught exceptions and continue serving potentially corrupt state. Preserve data integrity and use the runtime's recovery mechanism.
