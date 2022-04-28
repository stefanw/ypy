use crate::shared_types::SharedType;
use crate::type_conversions::py_into_any;
use crate::type_conversions::{events_into_py, ToPython};
use crate::y_transaction::YTransaction;
use lib0::any::Any;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::PyList;
use std::collections::HashMap;
use std::rc::Rc;
use yrs::types::text::TextEvent;
use yrs::types::Attrs;
use yrs::types::DeepObservable;
use yrs::{SubscriptionId, Text, Transaction};

/// A shared data type used for collaborative text editing. It enables multiple users to add and
/// remove chunks of text in efficient manner. This type is internally represented as a mutable
/// double-linked list of text chunks - an optimization occurs during `YTransaction.commit`, which
/// allows to squash multiple consecutively inserted characters together as a single chunk of text
/// even between transaction boundaries in order to preserve more efficient memory model.
///
/// `YText` structure internally uses UTF-8 encoding and its length is described in a number of
/// bytes rather than individual characters (a single UTF-8 code point can consist of many bytes).
///
/// Like all Yrs shared data types, `YText` is resistant to the problem of interleaving (situation
/// when characters inserted one after another may interleave with other peers concurrent inserts
/// after merging all updates together). In case of Yrs conflict resolution is solved by using
/// unique document id to determine correct and consistent ordering.
#[pyclass(unsendable)]
#[derive(Clone)]
pub struct YText(pub SharedType<Text, String>);
impl From<Text> for YText {
    fn from(v: Text) -> Self {
        YText(SharedType::new(v))
    }
}

#[pymethods]
impl YText {
    /// Creates a new preliminary instance of a `YText` shared data type, with its state initialized
    /// to provided parameter.
    ///
    /// Preliminary instances can be nested into other shared data types such as `YArray` and `YMap`.
    /// Once a preliminary instance has been inserted this way, it becomes integrated into Ypy
    /// document store and cannot be nested again: attempt to do so will result in an exception.
    #[new]
    pub fn new(init: Option<String>) -> Self {
        YText(SharedType::prelim(init.unwrap_or_default()))
    }

    /// Returns true if this is a preliminary instance of `YText`.
    ///
    /// Preliminary instances can be nested into other shared data types such as `YArray` and `YMap`.
    /// Once a preliminary instance has been inserted this way, it becomes integrated into Ypy
    /// document store and cannot be nested again: attempt to do so will result in an exception.
    #[getter]
    pub fn prelim(&self) -> bool {
        match self.0 {
            SharedType::Prelim(_) => true,
            _ => false,
        }
    }

    /// Returns an underlying shared string stored in this data type.
    pub fn __str__(&self) -> String {
        match &self.0 {
            SharedType::Integrated(v) => v.to_string(),
            SharedType::Prelim(v) => v.clone(),
        }
    }

    pub fn __repr__(&self) -> String {
        format!("YText({})", self.__str__())
    }

    /// Returns length of an underlying string stored in this `YText` instance,
    /// understood as a number of UTF-8 encoded bytes.
    pub fn __len__(&self) -> usize {
        match &self.0 {
            SharedType::Integrated(v) => v.len() as usize,
            SharedType::Prelim(v) => v.len(),
        }
    }

    /// Returns an underlying shared string stored in this data type.
    pub fn to_json(&self) -> String {
        let mut json_string = String::new();
        Any::String(self.__str__().into_boxed_str()).to_json(&mut json_string);
        json_string
    }

    /// Inserts a given `chunk` of text into this `YText` instance, starting at a given `index`.
    pub fn insert(
        &mut self,
        txn: &mut YTransaction,
        index: u32,
        chunk: &str,
        attributes: Option<HashMap<String, PyObject>>,
    ) -> PyResult<()> {
        let attributes: Option<PyResult<Attrs>> = attributes.map(Self::parse_attrs);

        if let Some(Ok(attributes)) = attributes {
            match &mut self.0 {
                SharedType::Integrated(text) => {
                    text.insert_with_attributes(txn, index, chunk, attributes);
                    Ok(())
                }
                SharedType::Prelim(_) => Err(PyTypeError::new_err("OOf")),
            }
        } else if let Some(Err(error)) = attributes {
            Err(error)
        } else {
            match &mut self.0 {
                SharedType::Integrated(text) => text.insert(txn, index, chunk),
                SharedType::Prelim(prelim_string) => {
                    prelim_string.insert_str(index as usize, chunk)
                }
            }
            Ok(())
        }
    }

    /// Inserts a given `embed` object into this `YText` instance, starting at a given `index`.
    ///
    /// Optional object with defined `attributes` will be used to wrap provided `embed`
    /// with a formatting blocks.`attributes` are only supported for a `YText` instance which
    /// already has been integrated into document store.
    pub fn insert_embed(
        &mut self,
        txn: &mut YTransaction,
        index: u32,
        embed: PyObject,
        attributes: Option<HashMap<String, PyObject>>,
    ) -> PyResult<()> {
        match &mut self.0 {
            SharedType::Integrated(text) => {
                let content = py_into_any(embed)
                    .ok_or(PyTypeError::new_err("Content could not be embedded"))?;
                if let Some(Ok(attrs)) = attributes.map(Self::parse_attrs) {
                    text.insert_embed_with_attributes(txn, index, content, attrs)
                } else {
                    text.insert_embed(txn, index, content)
                }
                Ok(())
            }
            SharedType::Prelim(_) => Err(PyTypeError::new_err(
                "Insert embeds requires YText instance to be integrated first.",
            )),
        }
    }

    /// Wraps an existing piece of text within a range described by `index`-`length` parameters with
    /// formatting blocks containing provided `attributes` metadata. This method only works for
    /// `YText` instances that already have been integrated into document store.
    pub fn format(
        &mut self,
        txn: &mut YTransaction,
        index: u32,
        length: u32,
        attributes: HashMap<String, PyObject>,
    ) -> PyResult<()> {
        match Self::parse_attrs(attributes) {
            Ok(attrs) => match &mut self.0 {
                SharedType::Integrated(text) => {
                    text.format(txn, index, length, attrs);
                    Ok(())
                }
                SharedType::Prelim(_) => Err(PyTypeError::new_err(
                    "Insert embeds requires YText instance to be integrated first.",
                )),
            },
            Err(err) => Err(err),
        }
    }

    /// Appends a given `chunk` of text at the end of current `YText` instance.
    pub fn push(&mut self, txn: &mut YTransaction, chunk: &str) {
        match &mut self.0 {
            SharedType::Integrated(v) => v.push(txn, chunk),
            SharedType::Prelim(v) => v.push_str(chunk),
        }
    }

    /// Deletes a specified range of of characters, starting at a given `index`.
    /// Both `index` and `length` are counted in terms of a number of UTF-8 character bytes.
    pub fn delete(&mut self, txn: &mut YTransaction, index: u32, length: u32) {
        match &mut self.0 {
            SharedType::Integrated(v) => v.remove_range(txn, index, length),
            SharedType::Prelim(v) => {
                v.drain((index as usize)..(index + length) as usize);
            }
        }
    }

    pub fn observe(&mut self, f: PyObject, deep: Option<bool>) -> PyResult<SubscriptionId> {
        let deep = deep.unwrap_or(false);
        match &mut self.0 {
            SharedType::Integrated(text) if deep => {
                let sub = text.observe_deep(move |txn, events| {
                    Python::with_gil(|py| {
                        let events = events_into_py(txn, events);
                        if let Err(err) = f.call1(py, (events,)) {
                            err.restore(py)
                        }
                    })
                });
                Ok(sub.into())
            }
            SharedType::Integrated(v) => Ok(v
                .observe(move |txn, e| {
                    Python::with_gil(|py| {
                        let e = YTextEvent::new(e, txn);
                        if let Err(err) = f.call1(py, (e,)) {
                            err.restore(py)
                        }
                    });
                })
                .into()),
            SharedType::Prelim(_) => Err(PyTypeError::new_err(
                "Cannot observe a preliminary type. Must be added to a YDoc first",
            )),
        }
    }
    /// Cancels the observer callback associated with the `subscripton_id`.
    pub fn unobserve(&mut self, subscription_id: SubscriptionId) -> PyResult<()> {
        match &mut self.0 {
            SharedType::Integrated(text) => {
                text.unobserve(subscription_id);
                Ok(())
            }
            SharedType::Prelim(_) => Err(PyTypeError::new_err(
                "Cannot unobserve a preliminary type. Must be added to a YDoc first",
            )),
        }
    }
}

impl YText {
    fn parse_attrs(attrs: HashMap<String, PyObject>) -> PyResult<Attrs> {
        attrs
            .into_iter()
            .map(|(k, v)| {
                let key = Rc::from(k);
                let value = py_into_any(v);
                if let Some(value) = value {
                    Ok((key, value))
                } else {
                    Err(PyTypeError::new_err(
                        "Cannot convert attributes into a standard type".to_string(),
                    ))
                }
            })
            .collect()
    }
}

/// Event generated by `YYText.observe` method. Emitted during transaction commit phase.
#[pyclass(unsendable)]
pub struct YTextEvent {
    inner: *const TextEvent,
    txn: *const Transaction,
    target: Option<PyObject>,
    delta: Option<PyObject>,
}

impl YTextEvent {
    pub fn new(event: &TextEvent, txn: &Transaction) -> Self {
        let inner = event as *const TextEvent;
        let txn = txn as *const Transaction;
        YTextEvent {
            inner,
            txn,
            target: None,
            delta: None,
        }
    }

    fn inner(&self) -> &TextEvent {
        unsafe { self.inner.as_ref().unwrap() }
    }

    fn txn(&self) -> &Transaction {
        unsafe { self.txn.as_ref().unwrap() }
    }
}

#[pymethods]
impl YTextEvent {
    /// Returns a current shared type instance, that current event changes refer to.
    #[getter]
    pub fn target(&mut self) -> PyObject {
        if let Some(target) = self.target.as_ref() {
            target.clone()
        } else {
            let target: PyObject =
                Python::with_gil(|py| YText::from(self.inner().target().clone()).into_py(py));
            self.target = Some(target.clone());
            target
        }
    }

    /// Returns an array of keys and indexes creating a path from root type down to current instance
    /// of shared type (accessible via `target` getter).
    pub fn path(&self) -> PyObject {
        Python::with_gil(|py| self.inner().path().into_py(py))
    }

    /// Returns a list of text changes made over corresponding `YText` collection within
    /// bounds of current transaction. These changes follow a format:
    ///
    /// - { insert: string, attributes: any|undefined }
    /// - { delete: number }
    /// - { retain: number, attributes: any|undefined }
    #[getter]
    pub fn delta(&mut self) -> PyObject {
        if let Some(delta) = &self.delta {
            delta.clone()
        } else {
            let delta: PyObject = Python::with_gil(|py| {
                let delta = self
                    .inner()
                    .delta(self.txn())
                    .into_iter()
                    .map(|d| d.clone().into_py(py));
                PyList::new(py, delta).into()
            });

            self.delta = Some(delta.clone());
            delta
        }
    }

    fn __str__(&self) -> String {
        format!(
            "YTextEvent(target={:?}, delta={:?})",
            self.target, self.delta
        )
    }

    fn __repr__(&self) -> String {
        self.__str__()
    }
}
