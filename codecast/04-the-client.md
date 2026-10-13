## The client
@ arch
The robot does not touch the drawer. It has a tiny program in its room called karl. Think of it as the window: you say your question through it, a paper comes back.
The window has no keys, no memory and no brain. It does not even know which questions exist. It passes your words through, and passes the answer back. That is all.
Why so dumb? Because it lives where the robot's commands run. If the robot is tricked, the worst it can do with the window is ask. Everything that matters is on the other side.
@ flow:karl::socket_path
Now the code. First, where is the window. socket_path tries three places.
! karl::socket_path/b1 karl::socket_path/b2
An environment variable wins. That is how you use it from a laptop over an ssh forward.
! karl::socket_path/b3 karl::socket_path/b4 karl::socket_path/b5
= crates/karl/src/main.rs:18:33-48
Else the sandbox path: Hermes binds the socket at this fixed place inside the robot's container.
! karl::socket_path/b6 karl::socket_path/b9
Else a socket named after the current user.
@ flow:karl::main
! karl::main/b5 karl::main/b7 karl::main/b10 karl::main/b11
main connects. If nobody answers, one line on standard error and exit 1. The robot learns the librarian is out, without a stack trace.
! karl::main/b15 karl::main/b16 karl::main/b17
The question is one line of JSON: a version number and the words exactly as typed. No grammar here. A new command is a daemon restart; this binary never changes.
! karl::main/b21 karl::main/b23 karl::main/b33 karl::main/b34
The answer starts with a header line: ok or not, how many bytes follow, and an exit code.
! karl::main/b35 karl::main/b36
Then exactly that many bytes of text are read. No guessing where the answer ends.
! karl::main/b40 karl::main/b41 karl::main/b43 karl::main/b45
= crates/karl/src/main.rs:76:5-28
Text to standard output when ok, to standard error otherwise, and the program exits with the librarian's code. Hermes shows exit codes to the robot, so a refusal reads as a refusal.
? Why not let the client check the command before sending?
  Because then there would be two parsers to keep in sync, and the one in the robot's room would be the one an attacker can study. One parser, in the daemon, is simpler and safer.
!
Let's follow that question through the window, to the librarian's side.
