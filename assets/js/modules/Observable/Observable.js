export class Observable {
    // When an Observable is created, it starts with no topics and no subscribers.
    constructor() {
        // "topics" holds a list of subscribers for each topic.
        this.topics = {};
    }

    /**
     * Add a subscriber to a specific topic.
     * @param {String} topic - The name of the topic to subscribe to.
     * @param {Object} subscriber - The object that wants to receive updates about this topic.
     */
    subscribe(topic, subscriber) {
        // If this topic doesn't exist yet, create an empty array for it.
        if (!this.topics[topic]) {
            this.topics[topic] = [];
        }
        // Add the subscriber to the topic's list.
        this.topics[topic].push(subscriber);
    }

    /**
     * Remove a subscriber from a specific topic.
     * @param {String} topic - The name of the topic to unsubscribe from.
     * @param {Object} subscriber - The object that no longer wants to receive updates about this topic.
     */
    unsubscribe(topic, subscriber) {
        // If the topic doesn't exist, or there are no subscribers, simply return.
        if (!this.topics[topic]) return;

        // Filter out the specified subscriber from the topic's list.
        this.topics[topic] = this.topics[topic].filter(obs => obs !== subscriber);
    }

    /**
     * Notify all subscribers of a specific topic.
     * @param {String} topic - The topic whose subscribers will be notified.
     * @param {Object} data - The data to be sent to the subscribers. Contains source and message.
     */
    notify(topic, data) {
        // If the topic doesn't exist, or there are no subscribers, simply return.
        if (!this.topics[topic]) return;

        // Notify each subscriber of this topic.
        this.topics[topic].forEach(subscriber => {
            // Do not notify the object that sent the update.
            if (subscriber !== data.source) {
                subscriber.update(data.message);
            }
        });
    }
}
