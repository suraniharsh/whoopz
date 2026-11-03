# Security

## What Whoopz is not

Whoopz is a **safety net, not a sandbox**. It helps you recover from mistakes. It does not stop
a program that wants to cause harm, and it is not a security boundary. A program can get past
it, for example through io_uring or by changing a path while the watcher reads it. Those are
known limits, not vulnerabilities.

## What to report

Report privately anything that lets Whoopz:

- lose or damage files it was meant to protect, or touch files outside what a command changed
- expose saved files (the stash) to other users on the machine
- run with more privileges than the user who started it

## How to report

Use GitHub's private vulnerability reporting: the **Security** tab → **Report a vulnerability**.
Please don't open a public issue for these.
