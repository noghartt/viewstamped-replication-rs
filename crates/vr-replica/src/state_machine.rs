use std::fmt::Debug;

pub trait StateMachine: Debug + Clone + 'static {
    type Input: Debug + Clone;
    type Output: Debug + Clone;

    fn apply(&self, input: Self::Input) -> Self::Output;
}
