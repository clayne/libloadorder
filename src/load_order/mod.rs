/*
 * This file is part of libloadorder
 *
 * Copyright (C) 2017 Oliver Hamlet
 *
 * libloadorder is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * libloadorder is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with libloadorder. If not, see <http://www.gnu.org/licenses/>.
 */

mod asterisk_based;
mod mutable;
mod readable;
#[cfg(test)]
mod tests;
mod textfile_based;
mod timestamp_based;
mod writable;

pub use load_order::asterisk_based::AsteriskBasedLoadOrder;
pub use load_order::readable::ReadableLoadOrder;
pub use load_order::textfile_based::TextfileBasedLoadOrder;
pub use load_order::timestamp_based::TimestampBasedLoadOrder;
pub use load_order::writable::WritableLoadOrder;
