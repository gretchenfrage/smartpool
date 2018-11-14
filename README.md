# Smartpool

Smartpool is a future-aware library designed to grant great control over the behavior 
of the pool.

### Features

* High-performance concurrency
* Future awareness
* Multiple priority levels
* Customizable task prioritization behavior
* Partitioning task channels by their behavior when the pool closes
* Scoped operations
* Scheduled operations
* Spatial prioritization, with the `smartpool-spatial` crate
* Use on stable channel

### Planned features

* More pool schedulers
  * Round-robin scheduler
* More channel prioritization schemes
  * Scalar priority
  * Shortest deadline first
* Associated thread state
* Formal performance tests

### Example

