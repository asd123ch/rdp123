# IronRDP DVC

This copy of upstream 0.10.0 limits each assembled dynamic-channel message
to 128 MiB before collecting fragments. It also checks the announced size
before decompressing a compressed first fragment. This accommodates a full
4096x4096 desktop with protocol overhead. Clipboard file streaming uses a
separate static channel and is not limited to this total file size.

RDP Dynamic Virtual Channel (DVC) support.

This crate is part of the [IronRDP] project.

[IronRDP]: https://github.com/Devolutions/IronRDP
