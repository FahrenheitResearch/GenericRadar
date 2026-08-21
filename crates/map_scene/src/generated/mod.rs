//! Compiled-in geographic source data.
//!
//! Generated from US Census cartographic boundaries and Natural Earth.
//! `map_scene` is its canonical owner; anything else that needs these
//! outlines borrows them from here rather than keeping a second copy.

pub mod basemap_data;
