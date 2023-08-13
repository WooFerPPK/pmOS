export class Observable {
    constructor() {
        this.topics = {};  // A dictionary where each topic corresponds to an array of observers
    }

    subscribe(topic, observer) {
        if (!this.topics[topic]) {
            this.topics[topic] = [];
        }
        this.topics[topic].push(observer);
    }

    unsubscribe(topic, observer) {
        if (!this.topics[topic]) return;
        this.topics[topic] = this.topics[topic].filter(obs => obs !== observer);
    }

    notify(topic, data) {
        if (!this.topics[topic]) return;
        this.topics[topic].forEach(observer => {
            if (observer !== data.source) {  // Do not notify the class that produced the update
                observer.update(data.message);
            }
        });
    }
}