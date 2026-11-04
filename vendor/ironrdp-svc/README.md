# IronRDP SVC

IronRDP traits to implement RDP static virtual channels.

This copy of upstream 0.10.0 bounds incoming static-channel messages to 64 MiB
before appending fragments, including compressed ones. The limit applies to
a single message, not the total size of a streamed clipboard file.

This crate is part of the [IronRDP] project.

[IronRDP]: https://github.com/Devolutions/IronRDP
