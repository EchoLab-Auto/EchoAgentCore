#!/usr/bin/env node
// EchoAgentCore 部署 CLI 入口。
import { main } from '../lib/cli.js';

main(process.argv.slice(2))
  .then((code) => {
    process.exitCode = code ?? 0;
  })
  .catch((error) => {
    console.error(`\n[echo-agent] ${error?.message ?? error}`);
    process.exitCode = 1;
  });
